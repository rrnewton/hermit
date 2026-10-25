#!/usr/bin/env -S rust-script --force
//! Exercise the complete trap backend with the patching engine absent.
//! This official validation producer keeps its Cargo targets, installation,
//! command logs and three OFF result records separate from the enabled cohort.
//! A fourth record belongs only to the mixed ON-CLI/OFF-runtime control.
//! ```cargo
//! [dependencies]
//! dagrun = { version = "0.15.0", git = "https://github.com/rrnewton/agent-utils.git", rev = "bfd648864b74d14b96cd3af9e36f04186aca7075" }
//! libc = "0.2"
//! serde_json = "1"
//! sha2 = "=0.10.9"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::TestResults;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

type Result<T> = std::result::Result<T, String>;
const FEATURES: &str = "in-guest-trap,exec-stop-launcher,native-exec";
const MANIFEST_TEST_COUNT: usize = 303;
const ISOLATION_TEST_COUNT: usize = 150;
const MIXED_RUNTIME_CASE: &str =
    "liteinst_cli_refuses_an_off_runtime_unless_patching_is_explicitly_disabled";
const ALLOCATOR_TESTS: &[&str] = &[
    "genuine_detcore_leaf_keeps_scope_free_work_private",
    "source_copy::compiled_source_transaction_closes_both_result_paths",
    "source_copy::gitless_standalone_compiler_sources",
    "standalone_legacy_tools_refuse_before_allocator_child_configuration",
];
const STACK_TESTS: &[&str] = &[
    "cold_constructor::caller_stack_samples_never_receive_constructor_placement_credit",
    "cold_constructor::genuine_detcore_constructor_uses_retained_disjoint_bootstrap_stack",
    "cold_constructor::later_guard_cannot_alias_the_retained_bootstrap_extent",
    "genuine_detcore_reaches_fixed_guarded_callback_and_altstack",
    "inert_preload_does_not_reserve_or_register_runtime_stacks",
    "stack_observation::tests::rejects_foreign_and_unreached_callback_records",
    "stack_observation::tests::rejects_unprobed_and_readable_guards",
    "standalone_legacy_tools_refuse_before_altstack_disabled_child_configuration",
    "standalone_legacy_tools_refuse_before_altstack_enabled_child_configuration",
];
const SHARED_IN_GUEST_CASES: [(&str, &str); 3] = [
    (
        "proc_fd_link_determinism",
        "proc_fd_link_aliases_and_truncation_in_guest_trap",
    ),
    (
        "socket_timestamp_determinism",
        "socket_receive_timestamps_use_logical_time_in_guest_trap",
    ),
    (
        "socket_ioctl_timestamp_determinism",
        "socket_timestamp_ioctls_use_logical_time_in_guest_trap",
    ),
];
type SuiteKey<'a> = (&'a str, &'a str, &'a str);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InventoryCounts {
    selected: usize,
    ignored: usize,
    filtered: usize,
    total: usize,
}

impl InventoryCounts {
    const fn new(selected: usize, ignored: usize, filtered: usize, total: usize) -> Self {
        Self {
            selected,
            ignored,
            filtered,
            total,
        }
    }
}

// Exact native OFF inventory for FEATURES and nextest_arguments. Library and
// binary suites remain distinct even when their binary names are equal.
const REQUIRED_NEXTEST_POPULATIONS: &[(SuiteKey<'static>, InventoryCounts)] = &[
    (
        ("hermit-run", "lib", "hermit"),
        InventoryCounts::new(478, 0, 0, 478),
    ),
    (
        ("hermit-run", "test", "cli"),
        InventoryCounts::new(573, 6, 120, 699),
    ),
    (
        ("hermit-run", "test", "liteinst_allocator"),
        InventoryCounts::new(4, 0, 0, 4),
    ),
    (
        ("hermit-run", "test", "liteinst_stacks"),
        InventoryCounts::new(9, 0, 0, 9),
    ),
    (
        ("hermit-run", "test", "proc_fd_link_determinism"),
        InventoryCounts::new(1, 0, 2, 3),
    ),
    (
        ("hermit-run", "test", "socket_ioctl_timestamp_determinism"),
        InventoryCounts::new(1, 0, 1, 2),
    ),
    (
        ("hermit-run", "test", "socket_timestamp_determinism"),
        InventoryCounts::new(1, 0, 2, 3),
    ),
    (
        ("hermit-run", "bin", "hermit"),
        InventoryCounts::new(613, 2, 0, 615),
    ),
];

const STREAM_LIMIT: u64 = 64 << 20;
// The enclosing DAG supplies CPU, memory and cgroup bounds. This deadline also
// bounds standalone producer use; pipe readers cap logs without applying an
// RLIMIT_FSIZE that would truncate Cargo's own object files.
const DEADLINE_SECONDS: u64 = 2300;
const PORTABLE_SKIPS: &[&str] = &[
    "run_kvm_",
    "backend_accepted_in_global_position",
    "run_dbt_strict_returns_with_blocked_stdin_source",
    "run_dbt_verifies_pipe_backpressure",
    "run_dbt_recovers_after_failed_exec",
    "run_dbt_verifies_queued_self_signals",
    "run_dbt_verifies_self_prlimit",
    "run_dbt_verifies_shell_process_lifecycle",
    "run_dbt_verifies_simple_env_shebang",
    "skid_overshoot_and_guest_failure_have_different_exit_codes",
    "run_ptrace_nonleader_exec_preserves_identity_and_time",
    "run_ptrace_nonleader_exec_preserves_preemption",
    "run_ptrace_nonleader_exec_displaces_runnable_leader",
    "run_ptrace_nonleader_exec_refuses_preemption_artifacts",
    "run_chaos_preemption_replay_reuses_the_recorded_epoch",
    "max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe",
    "poll_timeout_returns_on_time_while_another_thread_spins",
    "a_guests_sigchld_to_a_futex_waiting_sibling_beside_a_cpu_bound_one_ends_its_wait",
    "a_readied_pipe_poller_runs_once_the_actors_branch_counted_time_is_the_latency_later",
    "happens_before_relative_anchor_holds_the_first_call_after_its_base",
    "happens_before_relative_anchor_state_is_not_inherited_by_a_new_thread",
    "happens_before_unfired_relative_anchor_is_refused_with_its_base",
    "happens_before_hold_budget_ends_a_hold_a_spinner_never_releases",
    "happens_before_posthook_anchor_on_an_interrupted_call_is_refused_by_name",
    "happens_before_hold_in_a_vfork_child_is_refused_by_name",
    "cpu_profile_v3_guest_sees_the_v3_cpu_at_bitwise_parity",
    "cpu_profile_v3_host_loader_sees_exactly_x86_64_v3",
    "cpu_profile_v3_records_and_replays_the_v3_guest",
    "cpu_profile_v3_lazy_binding_xsave_is_identical_on_the_hashed_stack",
    "cpu_profile_v3_replay_refuses_a_recording_from_a_different_cpu",
    "cpu_profile_v3_cpuinfo_matches_cpuid",
];

fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

fn capture(mut input: impl Read, path: &Path, exceeded: &AtomicBool) -> Result<()> {
    let mut output = File::create_new(path).map_err(|e| e.to_string())?;
    let mut remaining = STREAM_LIMIT;
    let mut buffer = [0; 16384];
    loop {
        let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(());
        }
        let retained = count.min(remaining as usize);
        output
            .write_all(&buffer[..retained])
            .map_err(|e| e.to_string())?;
        remaining -= retained as u64;
        if retained != count {
            exceeded.store(true, Ordering::Release);
            // Keep draining until the parent kills the group; never allocate
            // or retain the overflowing stream.
        }
    }
}

struct Runner {
    root: PathBuf,
    evidence: PathBuf,
    environment: BTreeMap<String, PathBuf>,
    deadline: Instant,
    sequence: usize,
}

impl Runner {
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.current_dir(&self.root).envs(&self.environment);
        command.env("CARGO_NET_OFFLINE", "true");
        for name in [
            "HERMIT_PREPARED_NEXTEST_REQUIRED",
            "NEXTEST_PREPARED_BUILD_SELECTION",
            "NEXTEST_EXPECTED_EXECUTED",
            "HERMIT_NATIVE_EXEC_STAGE",
            "HERMIT_LITEINST_STAGE",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn run(&mut self, label: &str, command: &mut Command) -> Result<(ExitStatus, PathBuf)> {
        require(
            Instant::now() < self.deadline,
            "original OFF deadline expired before command launch",
        )?;
        self.sequence += 1;
        let directory = self.evidence.join(format!("{:02}-{label}", self.sequence));
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let arguments: Vec<_> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        fs::write(
            directory.join("argv.json"),
            serde_json::to_vec(&arguments).unwrap(),
        )
        .map_err(|e| e.to_string())?;
        eprintln!(
            "in-guest-trap without liteinst: {label}; logs {}",
            directory.display()
        );
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().map_err(|e| format!("{label}: {e}"))?;
        let pid = child.id() as i32;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let overflow = Arc::new(AtomicBool::new(false));
        let out_path = directory.join("stdout");
        let err_path = directory.join("stderr");
        let start = Instant::now();
        let status = thread::scope(|scope| -> Result<ExitStatus> {
            let out = scope.spawn(|| capture(stdout, &out_path, &overflow));
            let err = scope.spawn(|| capture(stderr, &err_path, &overflow));
            let mut refusal = None;
            let status = loop {
                if Instant::now() >= self.deadline || overflow.load(Ordering::Acquire) {
                    refusal = Some("producer deadline or 64-MiB stream limit reached");
                    // SAFETY: only this newly created child process group.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                    break child.wait().map_err(|e| e.to_string())?;
                }
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    // No descendant may retain a pipe after the command exits.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                    break status;
                }
                thread::sleep(Duration::from_millis(100));
            };
            out.join().map_err(|_| "stdout capture panicked")??;
            err.join().map_err(|_| "stderr capture panicked")??;
            require(
                !overflow.load(Ordering::Acquire),
                "64-MiB command stream limit reached",
            )?;
            if let Some(reason) = refusal {
                return Err(format!("{label}: {reason}"));
            }
            Ok(status)
        })?;
        fs::write(
            directory.join("status.json"),
            json!({
                "success":status.success(), "exit_code":status.code(),
                "elapsed_seconds":start.elapsed().as_secs_f64(),
            })
            .to_string(),
        )
        .map_err(|e| e.to_string())?;
        Ok((status, out_path))
    }

    fn checked(&mut self, label: &str, command: &mut Command) -> Result<PathBuf> {
        let (status, stdout) = self.run(label, command)?;
        require(
            status.success(),
            &format!(
                "{label} failed ({status}); retained logs beside {}",
                stdout.display()
            ),
        )?;
        Ok(stdout)
    }
}

fn graph_without_patching(text: &str) -> Result<BTreeSet<&str>> {
    let packages: BTreeSet<_> = text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    require(!packages.is_empty(), "Cargo tree selected no packages")?;
    require(
        !packages.contains("liteinst2"),
        "trap-only graph contains liteinst2",
    )?;
    // iced-x86 is independently used by Hermit's instruction decoder. It is
    // liteinst2, the patching engine, whose absence this product check proves.
    Ok(packages)
}

fn nextest_arguments() -> Vec<String> {
    let mut args: Vec<String> = [
        "--locked",
        "--offline",
        "--cargo-profile",
        "validate",
        "-p",
        "hermit-run",
        "--no-default-features",
        "--features",
        FEATURES,
        "--lib",
        "--bin",
        "hermit",
        "--test",
        "cli",
        "--test",
        "liteinst_allocator",
        "--test",
        "liteinst_stacks",
        "--test",
        "proc_fd_link_determinism",
        "--test",
        "socket_timestamp_determinism",
        "--test",
        "socket_ioctl_timestamp_determinism",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    // The three older integration binaries also contain unconditional DBT
    // legs. Select their separate OFF entrypoints without enabling DBT or
    // changing the existing enabled tests.
    let mut filters = vec![
        "binary(=cli)".to_owned(),
        "binary(=hermit)".to_owned(),
        "binary(=liteinst_allocator)".to_owned(),
        "binary(=liteinst_stacks)".to_owned(),
    ];
    for (binary, test) in SHARED_IN_GUEST_CASES {
        filters.push(format!("(binary(={binary}) & test(={test}))"));
    }
    args.extend(["-E".into(), filters.join(" | ")]);
    args.push("--".into());
    for skip in PORTABLE_SKIPS {
        args.extend(["--skip".into(), (*skip).into()]);
    }
    args
}

fn check_inventory(bytes: &[u8]) -> Result<BTreeSet<String>> {
    check_inventory_with_populations(bytes, REQUIRED_NEXTEST_POPULATIONS)
}

fn check_inventory_with_populations(
    bytes: &[u8],
    expected: &[(SuiteKey<'_>, InventoryCounts)],
) -> Result<BTreeSet<String>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let suites = value["rust-suites"]
        .as_object()
        .ok_or("Nextest suites missing")?;
    let mut selected = BTreeMap::<String, BTreeSet<String>>::new();
    let mut terminal_ids = BTreeSet::new();
    let mut actual = BTreeMap::new();
    let mut total_rows = 0usize;
    for (suite_id, suite) in suites {
        let package = suite["package-name"]
            .as_str()
            .ok_or("Nextest package name missing")?;
        let binary = suite["binary-name"]
            .as_str()
            .ok_or("Nextest binary name missing")?;
        let kind = suite["kind"]
            .as_str()
            .ok_or("Nextest binary kind missing")?;
        require(
            !package.is_empty() && !binary.is_empty() && matches!(kind, "lib" | "bin" | "test"),
            "unsupported Nextest package/binary/kind identity",
        )?;
        let binary_id = suite["binary-id"]
            .as_str()
            .ok_or("Nextest binary id missing")?;
        let expected_binary_id = match kind {
            "lib" => package.to_owned(),
            "bin" => format!("{package}::bin/{binary}"),
            "test" => format!("{package}::{binary}"),
            _ => unreachable!(),
        };
        require(
            suite_id == binary_id && binary_id == expected_binary_id,
            "Nextest suite key, binary id and package/kind/binary identity disagree",
        )?;
        require(
            suite["status"] == "listed",
            "required Nextest suite was not listed",
        )?;
        let cases = suite["testcases"]
            .as_object()
            .ok_or("Nextest testcases missing")?;
        let mut counts = InventoryCounts::new(0, 0, 0, cases.len());
        total_rows = total_rows
            .checked_add(cases.len())
            .ok_or("Nextest test count overflow")?;
        for (name, case) in cases {
            require(!name.is_empty(), "Nextest test name is empty")?;
            let ignored = case["ignored"]
                .as_bool()
                .ok_or("Nextest ignored marker missing")?;
            let status = case["filter-match"]["status"]
                .as_str()
                .ok_or("Nextest filter status missing")?;
            require(
                matches!(status, "matches" | "mismatch"),
                "unknown Nextest filter status",
            )?;
            if ignored {
                counts.ignored += 1;
            } else if status == "matches" {
                counts.selected += 1;
            } else {
                counts.filtered += 1;
            }
            if !ignored
                && (name.contains("in_guest")
                    || name.contains("liteinst")
                    || name.contains("native_exec"))
            {
                require(
                    status == "matches",
                    &format!("shared trap/native test was filtered: {binary}::{name}"),
                )?;
            }
            if status == "matches" && !ignored {
                // Match ci/nextest-test-results.rs::displayed_test_id exactly;
                // its CPU-attempt reconciliation does not prove that every
                // selected testcase reached a terminal record.
                let display_binary = if kind == "lib" {
                    package.to_owned()
                } else {
                    format!("{package}::{binary}")
                };
                require(
                    terminal_ids.insert(format!("{display_binary}${name}")),
                    "Nextest inventory repeats a terminal test identity",
                )?;
                selected
                    .entry(binary.into())
                    .or_default()
                    .insert(name.clone());
            }
        }
        require(
            actual.insert((package, kind, binary), counts).is_none(),
            "Nextest inventory repeats a package/kind/binary suite identity",
        )?;
    }
    require(
        value["test-count"].as_u64() == Some(total_rows as u64),
        "Nextest test-count differs from the sum of all suite testcase rows",
    )?;
    let expected_map = expected.iter().copied().collect::<BTreeMap<_, _>>();
    require(
        expected_map.len() == expected.len(),
        "required Nextest suite identity repeats",
    )?;
    require(
        actual == expected_map,
        &format!(
            "Nextest suite populations differ from the required native OFF inventory: actual {actual:?}; expected {expected_map:?}"
        ),
    )?;
    for (binary, name) in SHARED_IN_GUEST_CASES {
        require(
            selected.get(binary) == Some(&BTreeSet::from([name.to_owned()])),
            &format!("{binary}: required exactly the shared trap entrypoint {name}"),
        )?;
    }
    for (binary, names) in [
        ("liteinst_allocator", ALLOCATOR_TESTS),
        ("liteinst_stacks", STACK_TESTS),
    ] {
        let expected = names.iter().map(|name| (*name).to_owned()).collect();
        require(
            selected.get(binary) == Some(&expected),
            &format!("{binary}: selected fixture identities differ from their exact OFF scope"),
        )?;
    }
    let cli = selected
        .get("cli")
        .ok_or("portable CLI selection is empty")?;
    for name in [
        "in_guest_trap_sigalrm_scalar_identity_matches_ptrace",
        "in_guest_trap_dispatch_record_reports_no_patched_sites",
        "in_guest_trap_verifies_a_program_with_no_site_patched",
        "in_guest_trap_sets_site_patching_off_and_refuses_a_caller_who_turns_it_on",
        "in_guest_trap_keeps_site_patching_off_whatever_a_guest_library_does",
    ] {
        require(
            cli.iter()
                .any(|id| id == name || id.ends_with(&format!("::{name}"))),
            &format!("portable CLI selection lost {name}"),
        )?;
    }
    require(selected.values().flatten().any(|id| id.contains("native_exec_observer::tests::actual_observer_keeps_sequence_and_early_close_failure")),
        "native-exec observer tests are absent")?;
    Ok(terminal_ids)
}

fn read_report(path: &Path) -> Result<TestResults> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let report = TestResults::from_declared_schema_json_slice(&bytes, 2)?;
    require(
        report.executed_tests > 0,
        "a required child population executed zero tests",
    )?;
    Ok(report)
}

fn manifest_test_ids(rows: &[Value]) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for row in rows {
        require(
            row["backend"] == "in-guest-trap",
            "manifest plan contains another backend",
        )?;
        let test = row["test"].as_str().ok_or("manifest test id missing")?;
        let mode = row["mode"].as_str().ok_or("manifest mode missing")?;
        require(
            ids.insert(format!("{test} [in-guest-trap/{mode}]")),
            "manifest plan contains a duplicate cell",
        )?;
    }
    require(
        ids.len() == MANIFEST_TEST_COUNT,
        "required trap manifest population changed; update the plan and this exact count together",
    )?;
    Ok(ids)
}

fn isolation_test_ids(text: &str) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        require(
            line.starts_with("in-guest-trap:"),
            "isolation selection contains another backend or malformed cell",
        )?;
        require(
            ids.insert(format!("isolation/{line}")),
            "isolation selection contains a duplicate cell",
        )?;
    }
    require(
        ids.len() == ISOLATION_TEST_COUNT,
        "required trap isolation population changed; update its cells and this exact count together",
    )?;
    Ok(ids)
}

fn check_report_population(
    label: &str,
    report: &TestResults,
    expected: &BTreeSet<String>,
) -> Result<()> {
    let ids = report
        .results
        .as_ref()
        .ok_or("child report omitted terminal rows")?
        .iter()
        .map(|row| row.id.clone())
        .collect::<BTreeSet<_>>();
    require(
        report.executed_tests == expected.len() as u64 && ids == *expected,
        &format!("{label}: terminal population differs from its exact required identities"),
    )
}

fn mixed_runtime_inventory(bytes: &[u8]) -> Result<BTreeSet<String>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut ids = BTreeSet::new();
    for (suite_id, suite) in value["rust-suites"]
        .as_object()
        .ok_or("mixed-runtime Nextest suites missing")?
    {
        for (name, case) in suite["testcases"]
            .as_object()
            .ok_or("mixed-runtime Nextest cases missing")?
        {
            if case["filter-match"]["status"] != "matches" {
                continue;
            }
            require(
                suite["package-name"] == "hermit-run"
                    && suite["binary-name"] == "liteinst_mixed_runtime"
                    && suite["kind"] == "test"
                    && suite_id == "hermit-run::liteinst_mixed_runtime"
                    && suite["binary-id"] == suite_id.as_str()
                    && suite["status"] == "listed"
                    && name == MIXED_RUNTIME_CASE
                    && case["ignored"] == false,
                "mixed-runtime inventory selected a foreign, ignored or substituted case",
            )?;
            require(
                ids.insert(format!("{suite_id}${name}")),
                "duplicate mixed-runtime identity",
            )?;
        }
    }
    require(
        ids.len() == 1,
        "mixed-runtime inventory must select exactly one case",
    )?;
    Ok(ids)
}

fn merge_reports<const N: usize>(reports: [TestResults; N]) -> Result<TestResults> {
    let mut executed = 0u64;
    let mut filtered = 0u64;
    let mut rows = Vec::new();
    for report in reports {
        require(
            report.executed_tests > 0,
            "a required child population executed zero tests",
        )?;
        executed = executed
            .checked_add(report.executed_tests)
            .ok_or("executed count overflow")?;
        filtered = filtered
            .checked_add(report.filtered_tests)
            .ok_or("filtered count overflow")?;
        rows.extend(report.results.ok_or("child report omitted terminal rows")?);
    }
    // The shared constructor rejects duplicate identities and any malformed row.
    TestResults::current(executed, filtered, rows)
}

fn require_test_success<const N: usize>(runners: [bool; N], report: &TestResults) -> Result<()> {
    require(
        runners.into_iter().all(|ok| ok),
        "a test runner failed; preserved terminal rows cannot erase infrastructure failure",
    )?;
    require(
        report
            .results
            .as_ref()
            .ok_or("terminal rows missing")?
            .iter()
            .all(|row| row.passed),
        "an OFF cohort or the separate mixed ON/OFF control contains failing tests",
    )
}

// Keep build/install/program state in one reusable directory. The lock is never
// unlinked: every invocation opens the same inode and holds it through terminal
// reporting. Command/result evidence has its own retained generation outside
// target, so reusing the Cargo cache does not erase that evidence.
struct WorkDirectory {
    _lock: File,
    started: Instant,
    evidence: PathBuf,
    reuse: Value,
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let record = json!({
            "wall_seconds": self.started.elapsed().as_secs_f64(),
            "reuse": self.reuse,
        });
        if let Err(error) = fs::write(self.evidence.join("resources.json"), format!("{record}\n")) {
            eprintln!("cannot retain OFF resource measurement: {error}");
        }
        eprintln!("in-guest-trap OFF resource measurement: {record}");
    }
}

// The public HOST coordinator and its one private pinned phase share this
// process-independent epoch. Neither setup nor a phase transition resets it.
#[derive(Clone, Copy)]
struct SharedDeadline {
    started_ns: u64,
    deadline_ns: u64,
    instant: Instant,
}

fn monotonic_ns() -> Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is a valid writable timespec; CLOCK_MONOTONIC is system-wide.
    require(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0,
        "cannot read the original monotonic clock",
    )?;
    let seconds = u64::try_from(time.tv_sec).map_err(|_| "negative monotonic clock")?;
    let nanos = u64::try_from(time.tv_nsec).map_err(|_| "negative monotonic fraction")?;
    require(nanos < 1_000_000_000, "invalid monotonic fraction")?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(nanos))
        .ok_or_else(|| "monotonic clock overflow".into())
}

impl SharedDeadline {
    fn from_epoch(started_ns: u64, deadline_ns: u64) -> Result<Self> {
        let anchor = Instant::now();
        let now = monotonic_ns()?;
        require(started_ns <= now, "OFF epoch is in the future")?;
        require(
            started_ns.checked_add(DEADLINE_SECONDS * 1_000_000_000) == Some(deadline_ns),
            "private phase changed the original 2300-second deadline",
        )?;
        let remaining = deadline_ns
            .checked_sub(now)
            .filter(|n| *n > 0)
            .ok_or("original OFF deadline expired")?;
        Ok(Self {
            started_ns,
            deadline_ns,
            instant: anchor
                .checked_add(Duration::from_nanos(remaining))
                .ok_or("Instant deadline overflow")?,
        })
    }

    fn start() -> Result<Self> {
        let entry = monotonic_ns()?;
        let started = match std::env::var("DAGRUN_STEP_STARTED_MONOTONIC_NS") {
            Ok(value) => {
                require(
                    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                    "malformed scheduler monotonic epoch",
                )?;
                value
                    .parse()
                    .map_err(|_| "scheduler monotonic epoch overflow")?
            }
            Err(std::env::VarError::NotPresent) => entry,
            Err(_) => return Err("scheduler monotonic epoch is not UTF-8".into()),
        };
        require(
            started <= entry,
            "scheduler monotonic epoch is in the future",
        )?;
        Self::from_epoch(
            started,
            started
                .checked_add(DEADLINE_SECONDS * 1_000_000_000)
                .ok_or("scheduler deadline overflow")?,
        )
    }

    fn check(&self) -> Result<()> {
        require(
            Instant::now() < self.instant && monotonic_ns()? < self.deadline_ns,
            "original OFF deadline expired during setup, collection or source close",
        )
    }
}

const HOST_COMPAT_COUNT: usize = 62;
const PINNED_MANIFEST_COUNT: usize = 241;
const BRIDGE_SCHEMA: &str = "hermit-off-host-pinned/v1";
const SOURCE_INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "scripts/test-in-guest-trap-no-liteinst.rs",
    "scripts/lib/rust_script_prelude.rs",
    "ci/manifest-plan/src/validation_dag.rs",
    "ci/manifest-plan/src/validation_dag_static.rs",
    "ci/manifest-plan/src/bin/test-harness.rs",
    "ci/expected-e2e-plan.json",
    "tests/e2e/manifests/compat.yaml",
    "tests/compat/prepare_real_compat_fixtures.sh",
    "ci/hermetic/run-in-pinned-root.sh",
    "ci/hermetic/assert-no-network.sh",
    "ci/hermetic/assert-build-dependencies.sh",
    "ci/verify-packaged-detcore.py",
    "ci/build-liteinst-allocator-fixtures.rs",
    "ci/run-nextest-counted.sh",
    "ci/write-structured-test-counts.sh",
    "ci/conformance-test-counts.sh",
    "scripts/stage-liteinst-runtime.sh",
    "tests/isolation/conformance.rs",
    "tests/isolation/expected-mismatches.tsv",
    "liteinst-runtime-build/Cargo.toml",
    "liteinst-runtime-build/Cargo.lock",
    "liteinst-runtime-build/build.rs",
    "liteinst-runtime-build/artifact.rs",
    "hermit-cli/build_support.rs",
];

fn normal_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn text_field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .ok_or_else(|| format!("missing string field {name}"))
}

fn closed_fields(value: &Value, names: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or("handoff object is not a JSON object")?;
    require(
        object.len() == names.len() && object.keys().all(|k| names.contains(&k.as_str())),
        "unknown or missing request/handoff fields",
    )
}

fn read_json(path: &Path, clock: &SharedDeadline) -> Result<Value> {
    clock.check()?;
    let metadata = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    require(
        metadata.is_file() && metadata.len() <= STREAM_LIMIT,
        "bridge JSON is not a bounded regular file",
    )?;
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    clock.check()?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

// Publish complete JSON exclusively: polling can never see a partial final file.
fn publish_json(path: &Path, value: &Value, clock: &SharedDeadline) -> Result<()> {
    clock.check()?;
    let temporary = path.with_extension(format!("pending-{}", std::process::id()));
    let mut file = File::create_new(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    clock.check()?;
    fs::hard_link(&temporary, path).map_err(|e| e.to_string())?;
    fs::remove_file(&temporary).map_err(|e| e.to_string())?;
    clock.check()
}

fn file_record(path: &Path, logical: &Path, clock: &SharedDeadline) -> Result<Value> {
    clock.check()?;
    let link = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    require(
        before.is_file(),
        "bridge identity is not a regular readable file",
    )?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        clock.check()?;
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|e| e.to_string())?;
    let current = fs::metadata(path).map_err(|e| e.to_string())?;
    let stamp = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mode(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    require(
        stamp(&before) == stamp(&after) && stamp(&before) == stamp(&current),
        "bridge file changed while hashing",
    )?;
    let symlink = if link.file_type().is_symlink() {
        Some(fs::read_link(path).map_err(|e| e.to_string())?)
    } else {
        None
    };
    clock.check()?;
    Ok(
        json!({"path": logical, "sha256": format!("{:x}", hash.finalize()),
        "observed_size": before.len(), "observed_mode": before.mode(), "symlink_target": symlink}),
    )
}

fn source_records(root: &Path, clock: &SharedDeadline) -> Result<Value> {
    SOURCE_INPUTS
        .iter()
        .map(|relative| {
            let path = root.join(relative);
            require(
                fs::canonicalize(&path).map_err(|e| e.to_string())? == path,
                "source closure has a symlink escape",
            )?;
            file_record(&path, Path::new(relative), clock)
        })
        .collect::<Result<Vec<_>>>()
        .map(Value::Array)
}

fn installation_records(root: &Path, clock: &SharedDeadline) -> Result<Value> {
    fn visit(root: &Path, dir: &Path, rows: &mut Vec<Value>, clock: &SharedDeadline) -> Result<()> {
        clock.check()?;
        let mut paths = fs::read_dir(dir)
            .map_err(|e| e.to_string())?
            .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            clock.check()?;
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            let relative = path.strip_prefix(root).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() {
                rows.push(
                    json!({"relative":relative,"kind":"symlink","mode":metadata.mode(),
                    "target":fs::read_link(&path).map_err(|e| e.to_string())?}),
                );
            } else if metadata.is_dir() {
                rows.push(json!({"relative":relative,"kind":"directory","mode":metadata.mode()}));
                visit(root, &path, rows, clock)?;
            } else {
                require(metadata.is_file(), "installation has a non-file resource")?;
                rows.push(json!({"relative":relative,"kind":"file", "identity":file_record(&path,relative,clock)?}));
            }
        }
        Ok(())
    }
    require(
        fs::symlink_metadata(root)
            .map_err(|e| e.to_string())?
            .is_dir(),
        "OFF installation root is absent or not a real directory",
    )?;
    let mut rows = vec![json!({"relative":".","kind":"directory",
        "mode":fs::symlink_metadata(root).map_err(|e|e.to_string())?.mode()})];
    visit(root, root, &mut rows, clock)?;
    require(rows.len() > 1, "OFF installation is empty")?;
    Ok(Value::Array(rows))
}

fn manifest_partition(rows: &[Value]) -> Result<(BTreeSet<String>, Vec<Value>, Vec<Value>)> {
    let full = manifest_test_ids(rows)?;
    let mut pinned = Vec::new();
    let mut host = Vec::new();
    for row in rows {
        let category = row["category"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("authoritative manifest category missing")?;
        if category == "compat" {
            host.push(row.clone());
        } else {
            pinned.push(row.clone());
        }
    }
    require(
        host.len() == HOST_COMPAT_COUNT && pinned.len() == PINNED_MANIFEST_COUNT,
        "required manifest HOST62/pinned241 partition changed",
    )?;
    let host_ids = partition_plan_ids(&host)?;
    let pinned_ids = partition_plan_ids(&pinned)?;
    require(
        host_ids.is_disjoint(&pinned_ids)
            && host_ids
                .union(&pinned_ids)
                .cloned()
                .collect::<BTreeSet<_>>()
                == full,
        "manifest partition is not the exact disjoint 303-cell union",
    )?;
    Ok((full, pinned, host))
}

fn partition_plan_ids(rows: &[Value]) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for row in rows {
        require(
            row["backend"] == "in-guest-trap",
            "partition contains another backend",
        )?;
        let test = text_field(row, "test")?;
        let mode = text_field(row, "mode")?;
        require(
            !test.is_empty()
                && !mode.is_empty()
                && ids.insert(format!("{test} [in-guest-trap/{mode}]")),
            "empty or duplicate partition cell",
        )?;
    }
    Ok(ids)
}

fn check_partition_plan(actual: &[Value], expected: &[Value]) -> Result<BTreeSet<String>> {
    let actual_ids = partition_plan_ids(actual)?;
    let expected_ids = partition_plan_ids(expected)?;
    require(
        actual.len() == expected.len() && actual_ids == expected_ids,
        "selected manifest partition differs from its exact authoritative identities",
    )?;
    Ok(expected_ids)
}

fn authoritative_rows(root: &Path, clock: &SharedDeadline) -> Result<Vec<Value>> {
    let expected = read_json(&root.join("ci/expected-e2e-plan.json"), clock)?;
    Ok(expected["cells"]
        .as_array()
        .ok_or("authoritative manifest plan cells missing")?
        .iter()
        .filter(|row| row["backend"] == "in-guest-trap")
        .cloned()
        .collect())
}

fn identity_references(value: &Value, result: &mut Vec<(PathBuf, String)>) -> Result<()> {
    match value {
        Value::Object(object) => {
            if let (Some(path), Some(digest)) = (object.get("path"), object.get("sha256")) {
                let path = path.as_str().ok_or("FileIdentity path is not a string")?;
                let digest = digest
                    .as_str()
                    .ok_or("FileIdentity digest is not a string")?;
                require(
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "malformed mandatory FileIdentity digest",
                )?;
                result.push((PathBuf::from(path), digest.into()));
            }
            for child in object.values() {
                identity_references(child, result)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                identity_references(child, result)?;
            }
        }
        _ => (),
    }
    Ok(())
}

// Only maintained qualification records are selected, never staged.json's
// writer-only pre-rename observation or a scan of arbitrary Cargo JSON caches.
fn qualification_records(
    directory: &Path,
    clock: &SharedDeadline,
    found: &mut Vec<PathBuf>,
) -> Result<()> {
    clock.check()?;
    for entry in fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))? {
        clock.check()?;
        let path = entry.map_err(|e| e.to_string())?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.is_dir() {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or("non-UTF8 qualification directory")?;
            if !["deps", ".fingerprint", "incremental", "examples"].contains(&name) {
                qualification_records(&path, clock, found)?;
            }
        } else if path
            .file_name()
            .is_some_and(|n| n == "qualification.json" || n == "native-qualification.json")
        {
            require(
                metadata.is_file(),
                "qualification record is not a regular file",
            )?;
            found.push(path);
        }
    }
    Ok(())
}

fn retain_graph(roots: Vec<PathBuf>, generation: &Path, clock: &SharedDeadline) -> Result<Value> {
    let blobs = generation.join("proof-blobs");
    fs::create_dir(&blobs).map_err(|e| e.to_string())?;
    let mut pending = roots;
    let mut expected = BTreeMap::<PathBuf, String>::new();
    let mut failures = Vec::new();
    let mut rows = BTreeMap::<String, Value>::new();
    let mut attempts =
        File::create_new(generation.join("proof-attempts.jsonl")).map_err(|e| e.to_string())?;
    while let Some(path) = pending.pop() {
        clock.check()?;
        let name = path.to_str().ok_or("non-UTF8 proof path")?.to_owned();
        if rows.contains_key(&name) {
            continue;
        }
        let attempt = (|| -> Result<Value> {
            require(path.is_absolute(), "mandatory proof path is not absolute")?;
            let identity = file_record(&path, &path, clock)?;
            let digest = text_field(&identity, "sha256")?;
            if let Some(expected) = expected.get(&path) {
                require(
                    digest == expected,
                    "mandatory production FileIdentity changed",
                )?;
            }
            let copy = blobs.join(digest);
            if !copy.exists() {
                let mut input = File::open(&path).map_err(|e| e.to_string())?;
                let mut output = File::create_new(&copy).map_err(|e| e.to_string())?;
                let mut buffer = [0u8; 65536];
                loop {
                    clock.check()?;
                    let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
                    if count == 0 {
                        break;
                    }
                    output
                        .write_all(&buffer[..count])
                        .map_err(|e| e.to_string())?;
                }
                output.sync_all().map_err(|e| e.to_string())?;
            }
            require(
                file_record(&copy, &copy, clock)?["sha256"] == identity["sha256"],
                "retained proof copy differs from original",
            )?;
            require(
                file_record(&path, &path, clock)? == identity,
                "proof changed during retention",
            )?;
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let value = read_json(&path, clock)?;
                let mut refs = Vec::new();
                identity_references(&value, &mut refs)?;
                let mut reference_errors = Vec::new();
                for (reference, digest) in refs {
                    // Queue every known edge before reporting any earlier
                    // conflicting digest; a bad reference cannot erase peers.
                    pending.push(reference.clone());
                    if let Some(old) = expected.get(&reference) {
                        if old != &digest {
                            reference_errors.push(format!(
                                "conflicting mandatory FileIdentity: {}",
                                reference.display()
                            ));
                        }
                    } else {
                        expected.insert(reference.clone(), digest.clone());
                    }
                    if let Some(old) = reference.to_str().and_then(|path| rows.get(path)) {
                        if old["sha256"] != digest {
                            reference_errors.push(format!(
                                "previously retained reference changed: {}",
                                reference.display()
                            ));
                        }
                    }
                }
                require(reference_errors.is_empty(), &reference_errors.join("; "))?;
            }
            Ok(identity)
        })();
        if let Err(error) = writeln!(
            attempts,
            "{}",
            json!({"path":path,"result":attempt.as_ref().ok(),"error":attempt.as_ref().err()})
        ) {
            failures.push(format!(
                "retention attempt record {}: {error}",
                path.display()
            ));
        }
        match attempt {
            Ok(identity) => {
                rows.insert(name, identity);
            }
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }
    clock.check()?;
    let retained = Value::Array(rows.into_values().collect());
    if !failures.is_empty() {
        publish_json(
            &generation.join("proof-partial.json"),
            &json!({"records":retained,"errors":failures}),
            clock,
        )?;
        return Err(
            "mandatory production proof retention failed; all known remaining references attempted"
                .into(),
        );
    }
    Ok(retained)
}

fn pinned_to_host(
    root: &Path,
    generation: &Path,
    pinned_generation: &Path,
    path: &Path,
    source_paths: &BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    require(
        path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "noncanonical mandatory proof path",
    )?;
    if let Ok(relative) = path.strip_prefix("/src/target") {
        require(normal_relative(relative), "unsafe target proof path")?;
        let physical = root.join("ignored/hermetic/split/target").join(relative);
        require(
            fs::canonicalize(&physical).map_err(|e| e.to_string())? == physical,
            "target proof path escapes through a symlink",
        )?;
        return Ok(physical);
    }
    if let Ok(relative) = path.strip_prefix(pinned_generation) {
        require(normal_relative(relative), "unsafe generation proof path")?;
        let physical = generation.join(relative);
        require(
            fs::canonicalize(&physical).map_err(|e| e.to_string())? == physical,
            "generation proof path escapes through a symlink",
        )?;
        return Ok(physical);
    }
    // These are the two exact cache binds in run-in-pinned-root.sh. No
    // other /build path is evidence, and Cargo binaries/config are not mounted.
    for cache in ["git", "registry"] {
        if let Ok(relative) = path.strip_prefix(Path::new("/build/.cargo").join(cache)) {
            require(normal_relative(relative), "unsafe Cargo cache proof path")?;
            let physical = root
                .join("ignored/hermetic/split/cargo")
                .join(cache)
                .join(relative);
            require(
                fs::canonicalize(&physical).map_err(|e| e.to_string())? == physical,
                "Cargo cache proof path escapes through a symlink",
            )?;
            return Ok(physical);
        }
    }
    if path.starts_with("/nix/store") {
        return Ok(path.to_owned());
    }
    if source_paths.contains(path) {
        let relative = path.strip_prefix("/src").map_err(|e| e.to_string())?;
        require(
            normal_relative(relative)
                && !relative.starts_with("ignored")
                && !relative.starts_with("target"),
            "unsafe explicit source proof path",
        )?;
        let host = root.join(relative);
        require(
            fs::canonicalize(&host).map_err(|e| e.to_string())? == host,
            "explicit source proof path escapes through a symlink",
        )?;
        return Ok(host);
    }
    Err(format!(
        "unmapped mandatory pinned FileIdentity: {}",
        path.display()
    ))
}

fn verify_bridge(
    root: &Path,
    generation: &Path,
    pinned_generation: &Path,
    records: &Value,
    clock: &SharedDeadline,
) -> Result<()> {
    let rows = records.as_array().ok_or("proof graph is not an array")?;
    require(!rows.is_empty(), "production proof graph is empty")?;
    let mut identities = BTreeMap::new();
    let mut sources = BTreeSet::new();
    for row in rows {
        let path = PathBuf::from(text_field(row, "path")?);
        require(
            identities.insert(path.clone(), row).is_none(),
            "duplicate proof graph identity",
        )?;
        if let Ok(relative) = path.strip_prefix("/src") {
            if !relative.starts_with("target") && !relative.starts_with("ignored") {
                sources.insert(path);
            }
        }
    }
    for (logical, row) in &identities {
        clock.check()?;
        let physical = pinned_to_host(root, generation, pinned_generation, logical, &sources)?;
        require(
            file_record(&physical, logical, clock)? == **row,
            &format!("HOST bridge identity changed: {}", logical.display()),
        )?;
        let copy = generation
            .join("proof-blobs")
            .join(text_field(row, "sha256")?);
        require(
            file_record(&copy, &copy, clock)?["sha256"] == row["sha256"],
            "retained production proof copy changed",
        )?;
        if logical
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let mut refs = Vec::new();
            identity_references(&read_json(&physical, clock)?, &mut refs)?;
            for (path, digest) in refs {
                let identity = identities
                    .get(&path)
                    .ok_or_else(|| format!("mandatory graph edge omitted: {}", path.display()))?;
                require(
                    identity["sha256"] == digest,
                    "mandatory nested proof digest differs",
                )?;
            }
        }
    }
    Ok(())
}

struct PhaseContext {
    root: PathBuf,
    generation: PathBuf,
    work: PathBuf,
    clock: SharedDeadline,
    request: Value,
    request_sha256: String,
    entry_ns: u64,
}

fn phase_context(request_path: &Path) -> Result<PhaseContext> {
    let root = fs::canonicalize(".").map_err(|e| e.to_string())?;
    require(
        root == Path::new("/src"),
        "private OFF phase requires the maintained pinned root",
    )?;
    // Read only this bounded, regular request to learn its original epoch;
    // the private phase must never manufacture a new standalone deadline.
    let entry_ns = monotonic_ns()?;
    let metadata = fs::symlink_metadata(request_path).map_err(|e| e.to_string())?;
    require(
        metadata.is_file() && metadata.len() <= 65536,
        "private request is not a bounded regular file",
    )?;
    let request: Value =
        serde_json::from_slice(&fs::read(request_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    closed_fields(
        &request,
        &[
            "schema",
            "generation_relative",
            "started_monotonic_ns",
            "deadline_monotonic_ns",
            "lock_dev",
            "lock_ino",
            "sources",
        ],
    )?;
    require(
        request["schema"] == BRIDGE_SCHEMA,
        "unsupported OFF request schema",
    )?;
    let started = request["started_monotonic_ns"]
        .as_u64()
        .ok_or("missing original epoch")?;
    let deadline = request["deadline_monotonic_ns"]
        .as_u64()
        .ok_or("missing original deadline")?;
    require(
        started <= entry_ns,
        "private request epoch is later than phase entry",
    )?;
    let clock = SharedDeadline::from_epoch(started, deadline)?;
    if let Ok(value) = std::env::var("DAGRUN_STEP_STARTED_MONOTONIC_NS") {
        require(
            value.parse::<u64>().ok() == Some(started),
            "request differs from scheduler epoch",
        )?;
    }
    let relative = Path::new(text_field(&request, "generation_relative")?);
    require(
        normal_relative(relative)
            && relative.parent() == Some(Path::new("ignored/in-guest-trap-no-liteinst")),
        "unsafe or foreign OFF generation",
    )?;
    let generation = root.join(relative);
    require(
        fs::canonicalize(&generation).map_err(|e| e.to_string())? == generation
            && request_path == generation.join("request.json"),
        "request escaped its exact generation",
    )?;
    let work = root.join("target/ci/in-guest-trap/no-liteinst/work");
    let lock = fs::symlink_metadata(work.join("lock")).map_err(|e| e.to_string())?;
    require(
        lock.is_file()
            && request["lock_dev"].as_u64() == Some(lock.dev())
            && request["lock_ino"].as_u64() == Some(lock.ino()),
        "parent-owned fixed lock inode differs",
    )?;
    require(
        source_records(&root, &clock)? == request["sources"],
        "pinned source closure differs from HOST request",
    )?;
    let identity = file_record(request_path, request_path, &clock)?;
    Ok(PhaseContext {
        root,
        generation,
        work,
        clock,
        request,
        request_sha256: text_field(&identity, "sha256")?.to_owned(),
        entry_ns,
    })
}

// Cargo's actual CLI depfile names the OUT_DIR whose components were included
// by this CLI. Do not promote every historical OUT_DIR in a reused target.
fn depfile_paths(text: &str) -> Result<Vec<PathBuf>> {
    let first = text.lines().next().ok_or("CLI depfile is empty")?;
    let (_, inputs) = first
        .split_once(": ")
        .ok_or("CLI depfile lacks its target separator")?;
    let mut paths = Vec::new();
    let mut word = String::new();
    let mut escaped = false;
    for ch in inputs.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !word.is_empty() {
                paths.push(PathBuf::from(std::mem::take(&mut word)));
            }
        } else {
            word.push(ch);
        }
    }
    require(!escaped, "unfinished CLI depfile escape")?;
    if !word.is_empty() {
        paths.push(PathBuf::from(word));
    }
    Ok(paths)
}

fn proof_roots(
    context: &PhaseContext,
    installation: &Path,
    hermit: &Path,
    harness: &Path,
    planner: &Path,
) -> Result<Vec<PathBuf>> {
    let clock = &context.clock;
    let target = context.work.join("target");
    let mut roots = vec![
        hermit.to_owned(),
        harness.to_owned(),
        planner.to_owned(),
        installation.join("rsrcs/libdetcore_liteinst.so"),
        installation.join("rsrcs/libdetcore_liteinst.package.json"),
        target.join("validate/libreverie_liteinst.so"),
        target.join("validate/libreverie_liteinst.so.revision"),
    ];
    for pointer in [
        "m1-allocator-fixtures.path",
        "m2-stack-fixtures.path",
        "cold-stack-fixtures.path",
    ] {
        let pointer = context.root.join("target/ci/in-guest-trap").join(pointer);
        clock.check()?;
        let selected = PathBuf::from(
            fs::read_to_string(&pointer)
                .map_err(|e| e.to_string())?
                .trim(),
        );
        require(
            selected.is_absolute()
                && selected.starts_with(
                    context
                        .root
                        .join("target/ci/in-guest-trap/m1-allocator-fixtures"),
                ),
            "fixture pointer escaped the maintained OFF bundle root",
        )?;
        roots.push(pointer);
        // The complete selected producer envelope and every actual receipt are
        // required, including receipts represented by a PathBuf rather than FI.
        let bundle = selected.parent().ok_or("fixture bundle has no directory")?;
        for relative in [
            "bundle.json",
            "stack-bundle.json",
            "cold-bundle.json",
            "guest.json",
            "stack-guest.json",
            "cold-guest.json",
            "standalone/runtime.json",
            "detcore/runtime.json",
        ] {
            let path = bundle.join(relative);
            // Envelope names are read below for the actual guest-receipt paths;
            // only the three fixed envelopes and runtime receipts are fixed here.
            if ["guest.json", "stack-guest.json", "cold-guest.json"].contains(&relative) {
                continue;
            }
            roots.push(path);
        }
        let envelope = read_json(&selected, clock)?;
        fn receipt_paths(value: &Value, paths: &mut Vec<PathBuf>) {
            match value {
                Value::Object(object) => {
                    for (key, value) in object {
                        if key == "guest_receipt" {
                            if let Some(path) = value.as_str() {
                                paths.push(path.into());
                            }
                        }
                        receipt_paths(value, paths);
                    }
                }
                Value::Array(array) => {
                    for value in array {
                        receipt_paths(value, paths);
                    }
                }
                _ => (),
            }
        }
        receipt_paths(&envelope, &mut roots);
    }
    let depfile = target.join("validate/hermit.d");
    clock.check()?;
    let inputs = depfile_paths(&fs::read_to_string(&depfile).map_err(|e| e.to_string())?)?;
    let native_outs = inputs
        .iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|n| n == "native_exec_runner.bin")
        })
        .filter_map(|path| path.parent().map(Path::to_owned))
        .collect::<BTreeSet<_>>();
    require(
        native_outs.len() == 1,
        "actual CLI depfile does not identify one native producer OUT_DIR",
    )?;
    let out = native_outs.iter().next().unwrap();
    require(
        out.starts_with(target.join("validate/build")),
        "CLI native OUT_DIR escaped its exact target",
    )?;
    let mut component_hashes = BTreeMap::new();
    for (role, name) in [
        ("runner", "native_exec_runner.bin"),
        ("bootstrap", "native_exec_bootstrap.bin"),
        ("runtime", "native_exec_runtime.bin"),
    ] {
        let path = out.join(name);
        require(
            inputs.contains(&path),
            "CLI depfile omitted an actual native component",
        )?;
        component_hashes.insert(role, file_record(&path, &path, clock)?["sha256"].clone());
        roots.push(path);
    }
    roots.push(depfile);
    let mut candidates = Vec::new();
    qualification_records(&out.join("native-producer-target"), clock, &mut candidates)?;
    let mut matched_native = 0;
    for path in candidates {
        if path
            .file_name()
            .is_some_and(|n| n == "native-qualification.json")
        {
            let record = read_json(&path, clock)?;
            if component_hashes
                .iter()
                .all(|(role, digest)| record[*role]["sha256"] == *digest)
            {
                roots.push(path);
                matched_native += 1;
            }
        }
    }
    require(
        matched_native > 0,
        "actual included native components lack their complete matching qualification",
    )?;
    let standalone = target.join("validate/libreverie_liteinst.so");
    let digest = file_record(&standalone, &standalone, clock)?["sha256"].clone();
    let mut matched_standalone = 0;
    for entry in fs::read_dir(&context.work).map_err(|e| e.to_string())? {
        clock.check()?;
        let path = entry.map_err(|e| e.to_string())?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("standalone-target-"))
        {
            let mut candidates = Vec::new();
            qualification_records(&path, clock, &mut candidates)?;
            for path in candidates {
                if path.file_name().is_some_and(|n| n == "qualification.json")
                    && read_json(&path, clock)?["qualification"]["artifact"]["sha256"] == digest
                {
                    roots.push(path);
                    matched_standalone += 1;
                }
            }
        }
    }
    require(
        matched_standalone > 0,
        "actual staged standalone runtime lacks matching constructor qualification",
    )?;
    Ok(roots)
}

fn run_pinned_phase(request_path: &Path) -> Result<()> {
    let context = phase_context(request_path)?;
    let root = &context.root;
    let generation = &context.generation;
    let work = &context.work;
    let target = work.join("target");
    let installation = target.join("install_pkg");
    let hermit = target.join("validate/hermit");
    let evidence = generation.join("pinned-commands");
    fs::create_dir(&evidence).map_err(|e| e.to_string())?;
    let mut runner = Runner {
        root: root.clone(),
        evidence,
        environment: BTreeMap::from([
            ("CARGO_TARGET_DIR".into(), target.clone()),
            ("HERMIT_INSTALL_DIR".into(), installation.clone()),
            ("HERMIT_BIN".into(), hermit.clone()),
            ("HERMIT_LITEINST_TEST_BINARY".into(), hermit.clone()),
            ("E2E_BUILD_ROOT".into(), work.join("programs")),
            ("E2E_RESULT_ROOT".into(), generation.join("e2e")),
            (
                "E2E_RUN_ID".into(),
                PathBuf::from(format!(
                    "trap-only-{}",
                    text_field(&context.request, "generation_relative")?
                        .rsplit('/')
                        .next()
                        .ok_or("missing generation leaf")?
                )),
            ),
        ]),
        deadline: context.clock.instant,
        sequence: 0,
    };
    let jobs = std::env::var("CARGO_BUILD_JOBS").unwrap_or_else(|_| "8".into());
    let jobs: usize = jobs
        .parse()
        .map_err(|_| "CARGO_BUILD_JOBS must be a positive integer")?;
    require(jobs > 0, "CARGO_BUILD_JOBS must be a positive integer")?;
    let workers = jobs.min(8).to_string();
    runner
        .environment
        .insert("NEXTEST_TEST_THREADS".into(), PathBuf::from(&workers));
    for (label, package, features, manifest) in [
        (
            "graph-cli",
            "hermit-run",
            Some("in-guest-trap,exec-stop-launcher"),
            "Cargo.toml",
        ),
        ("graph-native", "hermit-run", Some(FEATURES), "Cargo.toml"),
        ("graph-detcore", "detcore-liteinst", None, "Cargo.toml"),
        ("graph-install", "hermit-install", None, "Cargo.toml"),
        (
            "graph-standalone",
            "hermit-liteinst-runtime-build",
            None,
            "liteinst-runtime-build/Cargo.toml",
        ),
    ] {
        let mut command = runner.command("cargo");
        command.args([
            "tree",
            "--locked",
            "--offline",
            "--manifest-path",
            manifest,
            "-p",
            package,
            "--no-default-features",
            "--edges",
            "normal,build,dev",
            "--prefix",
            "none",
        ]);
        if let Some(features) = features {
            command.args(["--features", features]);
        }
        let graph = runner.checked(label, &mut command)?;
        let graph = fs::read_to_string(graph).map_err(|e| e.to_string())?;
        let packages = graph_without_patching(&graph)?;
        if label == "graph-cli" || label == "graph-native" {
            for name in [
                "reverie-liteinst",
                "reverie-inguest",
                "reverie-rpc-transport",
                "reverie-loader",
            ] {
                require(
                    packages.contains(name),
                    &format!("{label} lost required {name}"),
                )?;
            }
        }
    }
    let mut command = runner.command("cargo");
    command.args([
        "build",
        "--locked",
        "--offline",
        "-p",
        "hermit-run",
        "--no-default-features",
        "--features",
        FEATURES,
        "--profile",
        "validate",
        "--bins",
    ]);
    runner.checked("build-cli-native", &mut command)?;
    let mut command = runner.command("./ci/run-with-reverie-dbt-budget.sh");
    command.args([
        "cargo",
        "build",
        "--locked",
        "--offline",
        "-p",
        "hermit-install",
        "--no-default-features",
        "--profile",
        "validate",
        "--bin",
        "stage-detcore-liteinst",
    ]);
    runner.checked("build-install", &mut command)?;
    let mut command = runner.command(target.join("validate/stage-detcore-liteinst"));
    command.arg("--git").arg(&target);
    runner.checked("finalize-detcore", &mut command)?;
    let mut command = runner.command("python3");
    command
        .args(["ci/verify-packaged-detcore.py", "--without-liteinst"])
        .arg(&installation);
    runner.checked("verify-package", &mut command)?;
    let standalone = target.join("validate/libreverie_liteinst.so");
    let mut command = runner.command("./scripts/stage-liteinst-runtime.sh");
    command
        .arg("release")
        .arg(&standalone)
        .arg(work.join("standalone-target"))
        .arg("--without-liteinst");
    runner.checked("stage-standalone", &mut command)?;
    let mut command = runner.command("./ci/build-liteinst-allocator-fixtures.rs");
    command.arg("--without-liteinst");
    runner.checked("qualify-fixtures", &mut command)?;
    for (variable, pointer) in [
        (
            "HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE",
            "m1-allocator-fixtures.path",
        ),
        ("HERMIT_M2_STACK_FIXTURE_BUNDLE", "m2-stack-fixtures.path"),
        (
            "HERMIT_COLD_STACK_FIXTURE_BUNDLE",
            "cold-stack-fixtures.path",
        ),
    ] {
        runner.environment.insert(
            variable.into(),
            root.join("target/ci/in-guest-trap").join(pointer),
        );
    }
    let mut command = runner.command("./ci/build-liteinst-allocator-fixtures.rs");
    command.args(["--without-liteinst", "--print-standalone"]);
    let fixture = runner.checked("fixture-standalone", &mut command)?;
    let fixture = fs::read_to_string(fixture).map_err(|e| e.to_string())?;
    runner.environment.insert(
        "REVERIE_LITEINST_PRELOAD".into(),
        PathBuf::from(fixture.trim()),
    );
    let mut command = runner.command("cargo");
    command.args([
        "build",
        "--locked",
        "--offline",
        "-p",
        "hermetic_infra_hermit_tests",
        "--bins",
        "--profile",
        "validate",
    ]);
    runner.checked("build-guest-programs", &mut command)?;
    let args = nextest_arguments();
    let mut command = runner.command("cargo");
    command
        .args(["nextest", "list", "--message-format", "json"])
        .args(&args);
    let inventory = runner.checked("nextest-inventory", &mut command)?;
    let nextest_inventory = inventory.clone();
    let nextest_ids = check_inventory(&fs::read(&inventory).map_err(|e| e.to_string())?)?;
    let nextest_report = generation.join("nextest-results.json");
    let mut command = runner.command("./ci/run-nextest-counted.sh");
    if std::env::var_os("CI").is_some() {
        command.args(["--profile", "ci"]);
    }
    command
        .args(&args)
        .env("DAGRUN_TEST_COUNTS_PATH", &nextest_report);
    let (nextest_status, nextest_stdout) = runner.run("nextest-run", &mut command)?;
    // This independent cohort builds its own harness too. Depending on the
    // ordinary pinned harness would make the Buck E2E selection retain a
    // producer it deliberately replaces.
    let mut command = runner.command("cargo");
    command.args([
        "build",
        "--locked",
        "--offline",
        "-p",
        "hermit-manifest-plan",
        "--bin",
        "test-harness",
        "--bin",
        "hermit-manifest-plan",
        "--profile",
        "validate",
    ]);
    runner.checked("build-manifest-harness", &mut command)?;
    let harness = target.join("validate/test-harness");
    let expected: Value = serde_json::from_slice(
        &fs::read(root.join("ci/expected-e2e-plan.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let expected_rows = expected["cells"]
        .as_array()
        .ok_or("authoritative manifest plan cells missing")?
        .iter()
        .filter(|row| row["backend"] == "in-guest-trap")
        .cloned()
        .collect::<Vec<_>>();
    let (_, pinned_rows, _) = manifest_partition(&expected_rows)?;
    let manifest_ids = partition_plan_ids(&pinned_rows)?;
    let mut command = runner.command(&harness);
    command.args([
        "plan",
        "--backend",
        "in-guest-trap",
        "--ci-only",
        "--exclude-category",
        "compat",
        "--format",
        "json",
    ]);
    let plan = runner.checked("manifest-plan", &mut command)?;
    let pinned_plan = plan.clone();
    let plan: Value = serde_json::from_slice(&fs::read(&plan).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    check_partition_plan(
        plan.as_array()
            .ok_or("selected manifest plan is not an array")?,
        &pinned_rows,
    )?;
    let mut command = runner.command(&harness);
    command.args([
        "build",
        "--backend",
        "in-guest-trap",
        "--ci-only",
        "--exclude-category",
        "compat",
        "--jobs",
        &workers,
    ]);
    runner.checked("build-manifest-programs", &mut command)?;
    let e2e_report = generation.join("e2e-results-pinned.json");
    let mut command = runner.command(&harness);
    command
        .args([
            "run",
            "--backend",
            "in-guest-trap",
            "--ci-only",
            "--exclude-category",
            "compat",
            "--prebuilt",
            "--jobs",
            &workers,
            "--results",
        ])
        .arg(generation.join("cells-pinned.jsonl"))
        .env("DAGRUN_TEST_COUNTS_PATH", &e2e_report);
    let (e2e_status, e2e_stdout) = runner.run("manifest-run", &mut command)?;
    // The isolation matrix is an independent required cohort, outside the E2E
    // manifests. Keep its existing full assertion scope and mismatch ledger.
    let mut command = runner.command("./tests/isolation/conformance.rs");
    command.args(["--backends", "in-guest-trap", "--list"]);
    let inventory = runner.checked("isolation-inventory", &mut command)?;
    let isolation_inventory = inventory.clone();
    let isolation_ids =
        isolation_test_ids(&fs::read_to_string(&inventory).map_err(|e| e.to_string())?)?;
    let isolation_directory = generation.join("isolation");
    let isolation_report = generation.join("isolation-results.json");
    let mut command = runner.command("./ci/conformance-test-counts.sh");
    command
        .arg(&isolation_directory)
        .args(["--", "./tests/isolation/conformance.rs", "--hermit"])
        .arg(&hermit)
        .arg("--installation")
        .arg(&installation)
        .args([
            "--executor",
            "local",
            "--expected",
            "tests/isolation/expected-mismatches.tsv",
            "--jobs",
            &workers,
            "--backends",
            "in-guest-trap",
            "--artifacts",
        ])
        .arg(&isolation_directory)
        .env("DAGRUN_TEST_COUNTS_PATH", &isolation_report);
    let (isolation_status, isolation_stdout) = runner.run("isolation-run", &mut command)?;
    // This ON control consumes the actual OFF installation just verified
    // above. A separate target prevents ON features from unifying with any
    // OFF producer; the same work lock spans both configurations and reporting.
    let mixed_target = work.join("mixed-on-target");
    let mixed_hermit = mixed_target.join("validate/hermit");
    let mixed_report = generation.join("mixed-on-results.json");
    let mixed_arguments = [
        "--locked",
        "--offline",
        "--cargo-profile",
        "validate",
        "-p",
        "hermit-run",
        "--no-default-features",
        "--features",
        "liteinst,mixed-runtime-test-support",
        "--test",
        "liteinst_mixed_runtime",
        "-E",
        "test(=liteinst_cli_refuses_an_off_runtime_unless_patching_is_explicitly_disabled)",
    ];
    let mut command = runner.command("cargo");
    command
        .args(["nextest", "list"])
        .args(mixed_arguments)
        .args(["--message-format", "json"])
        .env("CARGO_TARGET_DIR", &mixed_target);
    let inventory = runner.checked("mixed-on-inventory", &mut command)?;
    let mixed_inventory = inventory.clone();
    let mixed_ids = mixed_runtime_inventory(&fs::read(&inventory).map_err(|e| e.to_string())?)?;
    let mut command = runner.command("./ci/run-nextest-counted.sh");
    command
        .args(mixed_arguments)
        .env("CARGO_TARGET_DIR", &mixed_target)
        .env("HERMIT_BIN", &mixed_hermit)
        .env("HERMIT_LITEINST_TEST_BINARY", &mixed_hermit)
        .env("HERMIT_MIXED_LITEINST_OFF_INSTALLATION", &installation)
        .env("NEXTEST_EXPECTED_EXECUTED", "1")
        .env("DAGRUN_TEST_COUNTS_PATH", &mixed_report);
    let (mixed_status, mixed_stdout) = runner.run("mixed-on-run", &mut command)?;
    // All real raw records are handed off before a known test verdict is
    // returned. A complete failing pinned phase does not suppress HOST62.
    let reports = [
        read_report(&nextest_report)?,
        read_report(&e2e_report)?,
        read_report(&isolation_report)?,
        read_report(&mixed_report)?,
    ];
    let populations = [
        check_report_population("Nextest", &reports[0], &nextest_ids),
        check_report_population("pinned manifest", &reports[1], &manifest_ids),
        check_report_population("isolation", &reports[2], &isolation_ids),
        check_report_population("mixed ON/OFF", &reports[3], &mixed_ids),
    ];
    let raw = json!({
        "nextest_inventory":nextest_inventory,"isolation_inventory":isolation_inventory,
        "mixed_inventory":mixed_inventory,"manifest_plan":pinned_plan,
        "nextest_report":nextest_report,"manifest_report":e2e_report,
        "isolation_report":isolation_report,"mixed_report":mixed_report,
        "manifest_cells":generation.join("cells-pinned.jsonl"),
        "nextest_status":nextest_stdout.parent().ok_or("missing command log parent")?.join("status.json"),
        "manifest_status":e2e_stdout.parent().ok_or("missing command log parent")?.join("status.json"),
        "isolation_status":isolation_stdout.parent().ok_or("missing command log parent")?.join("status.json"),
        "mixed_status":mixed_stdout.parent().ok_or("missing command log parent")?.join("status.json"),
    });
    let planner = target.join("validate/hermit-manifest-plan");
    let mut roots = proof_roots(&context, &installation, &hermit, &harness, &planner)?;
    roots.extend(
        raw.as_object()
            .ok_or("raw handoff object missing")?
            .values()
            .map(|value| {
                value
                    .as_str()
                    .map(PathBuf::from)
                    .ok_or("raw record path missing")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );
    // Retain each original command, both streams and actual status, including
    // producer commands whose output is not itself a FileIdentity edge.
    for entry in fs::read_dir(&runner.evidence).map_err(|e| e.to_string())? {
        let directory = entry.map_err(|e| e.to_string())?.path();
        for name in ["argv.json", "stdout", "stderr", "status.json"] {
            roots.push(directory.join(name));
        }
    }
    let tree = installation_records(&installation, &context.clock)?;
    for row in tree.as_array().ok_or("installation census missing")? {
        if row["kind"] == "file" {
            roots.push(installation.join(text_field(row, "relative")?));
        }
    }
    let runtime_environment = [
        "HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE",
        "HERMIT_M2_STACK_FIXTURE_BUNDLE",
        "HERMIT_COLD_STACK_FIXTURE_BUNDLE",
        "REVERIE_LITEINST_PRELOAD",
    ]
    .into_iter()
    .map(|name| {
        let value = runner
            .environment
            .get(name)
            .ok_or("pinned fixture environment missing")?;
        roots.push(value.clone());
        Ok((name.to_owned(), json!(value)))
    })
    .collect::<Result<serde_json::Map<String, Value>>>()?;
    let records = retain_graph(roots, generation, &context.clock)?;
    let after = source_records(root, &context.clock)?;
    require(
        after == context.request["sources"],
        "pinned source changed before handoff",
    )?;
    let handoff = json!({"schema":BRIDGE_SCHEMA,"request_sha256":context.request_sha256,
        "generation_relative":context.request["generation_relative"],
        "started_monotonic_ns":context.clock.started_ns,"deadline_monotonic_ns":context.clock.deadline_ns,
        "lock_dev":context.request["lock_dev"],"lock_ino":context.request["lock_ino"],
        "sources_before":context.request["sources"],"sources_after":after,
        "observed_entry_monotonic_ns":context.entry_ns,"observed_close_monotonic_ns":monotonic_ns()?,
        "installation":tree,"records":records,"raw":raw,
        "runtime_environment":runtime_environment});
    publish_json(&generation.join("handoff.json"), &handoff, &context.clock)?;
    for population in populations {
        population?;
    }
    let report = merge_reports(reports)?;
    require_test_success(
        [
            nextest_status.success(),
            e2e_status.success(),
            isolation_status.success(),
            mixed_status.success(),
        ],
        &report,
    )?;
    context.clock.check()
}

const PINNED_PHASE_ENV: &[&str] = &[
    "CARGO_BUILD_JOBS",
    "CI",
    "DAGRUN_STEP_STARTED_MONOTONIC_NS",
    "E2E_BUILD_ROOT",
    "E2E_KERNEL_VERSION",
    "E2E_MACHINE_SHORTNAME",
    "E2E_PARITY_POST_PASS",
    "E2E_PARITY_SELECT",
    "E2E_RESULT_ROOT",
    "E2E_RUN_ID",
    "HERMIT_E2E_EMPTY_WORKDIR",
    "HERMIT_EPOCH",
    "HERMIT_REQUIRE_DAP",
    "HERMIT_REQUIRE_DAP_GDB",
    "HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER",
    "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER",
    "HERMIT_VALIDATE_BUCK_DOTSLASH",
    "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT",
    "HERMIT_VALIDATE_RELEASE_BUILD_MODE",
    "L4_REPS",
    "NEXTEST_TEST_THREADS",
    "PR_NUMBER",
    "RUST_MIN_STACK",
    "SUPER_REPETITIONS",
    "THIRD_PARTY_BUILD_JOBS",
    "VALIDATE_RUN_STATE",
    "VALIDATE_VERBOSITY",
];
const PINNED_PHASE_SHELL: &str = r#"/src/ci/hermetic/assert-no-network.sh &&
/src/ci/hermetic/assert-build-dependencies.sh &&
export PATH="/src/ci/rust-script-bin:$PATH" &&
export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT=/src/target/ci/rust-scripts &&
export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1 &&
exec /src/scripts/test-in-guest-trap-no-liteinst.rs --pinned-phase "$1""#;

fn host_raw_paths(
    root: &Path,
    generation: &Path,
    pinned_generation: &Path,
    raw: &Value,
) -> Result<BTreeMap<String, PathBuf>> {
    closed_fields(
        raw,
        &[
            "nextest_inventory",
            "isolation_inventory",
            "mixed_inventory",
            "manifest_plan",
            "nextest_report",
            "manifest_report",
            "isolation_report",
            "mixed_report",
            "manifest_cells",
            "nextest_status",
            "manifest_status",
            "isolation_status",
            "mixed_status",
        ],
    )?;
    let mut paths = BTreeMap::new();
    for (name, leaf) in [
        ("nextest_report", "nextest-results.json"),
        ("manifest_report", "e2e-results-pinned.json"),
        ("isolation_report", "isolation-results.json"),
        ("mixed_report", "mixed-on-results.json"),
        ("manifest_cells", "cells-pinned.jsonl"),
    ] {
        require(
            Path::new(text_field(raw, name)?) == pinned_generation.join(leaf),
            "handoff substituted a cohort result path",
        )?;
    }
    for (name, label, leaf) in [
        ("nextest_inventory", "nextest-inventory", "stdout"),
        ("isolation_inventory", "isolation-inventory", "stdout"),
        ("mixed_inventory", "mixed-on-inventory", "stdout"),
        ("manifest_plan", "manifest-plan", "stdout"),
        ("nextest_status", "nextest-run", "status.json"),
        ("manifest_status", "manifest-run", "status.json"),
        ("isolation_status", "isolation-run", "status.json"),
        ("mixed_status", "mixed-on-run", "status.json"),
    ] {
        let path = Path::new(text_field(raw, name)?);
        let parent = path.parent().ok_or("raw command path has no parent")?;
        let label_match = parent
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split_once('-'))
            .is_some_and(|(number, actual)| {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) && actual == label
            });
        require(
            parent.parent() == Some(pinned_generation.join("pinned-commands").as_path())
                && path.file_name().is_some_and(|name| name == leaf)
                && label_match,
            "handoff substituted a raw inventory or actual runner status",
        )?;
    }
    for (name, value) in raw.as_object().ok_or("raw paths missing")? {
        let logical = Path::new(value.as_str().ok_or("raw path is not a string")?);
        paths.insert(
            name.clone(),
            pinned_to_host(
                root,
                generation,
                pinned_generation,
                logical,
                &BTreeSet::new(),
            )?,
        );
    }
    Ok(paths)
}

fn verify_work_lock(work: &Path, request: &Value, clock: &SharedDeadline) -> Result<()> {
    clock.check()?;
    let metadata = fs::symlink_metadata(work.join("lock")).map_err(|e| e.to_string())?;
    require(
        metadata.is_file()
            && request["lock_dev"].as_u64() == Some(metadata.dev())
            && request["lock_ino"].as_u64() == Some(metadata.ino()),
        "the continuously held OFF lock inode was replaced",
    )
}

fn run_host_locked(
    root: &Path,
    generation: &Path,
    work: &Path,
    request: &Value,
    clock: &SharedDeadline,
    writer: &Path,
) -> Result<()> {
    let relative = Path::new(text_field(request, "generation_relative")?);
    let pinned_generation = Path::new("/src").join(relative);
    let target = work.join("target");
    let installation = target.join("install_pkg");
    let hermit = target.join("validate/hermit");
    let harness = target.join("validate/test-harness");
    let planner = target.join("validate/hermit-manifest-plan");
    let evidence = generation.join("host-commands");
    fs::create_dir(&evidence).map_err(|e| e.to_string())?;
    let mut runner = Runner {
        root: root.to_owned(),
        evidence,
        environment: BTreeMap::from([
            ("CARGO_TARGET_DIR".into(), target.clone()),
            ("HERMIT_INSTALL_DIR".into(), installation.clone()),
            ("HERMIT_BIN".into(), hermit.clone()),
            ("HERMIT_LITEINST_TEST_BINARY".into(), hermit.clone()),
            ("E2E_BUILD_ROOT".into(), work.join("programs")),
            ("E2E_RESULT_ROOT".into(), generation.join("e2e-host")),
            (
                "E2E_RUN_ID".into(),
                PathBuf::from(format!(
                    "trap-only-{}-host",
                    relative
                        .file_name()
                        .and_then(|s| s.to_str())
                        .ok_or("generation leaf missing")?
                )),
            ),
            ("HERMIT_E2E_EMPTY_WORKDIR".into(), PathBuf::from("/test")),
        ]),
        deadline: clock.instant,
        sequence: 0,
    };
    let jobs: usize = std::env::var("CARGO_BUILD_JOBS")
        .unwrap_or_else(|_| "8".into())
        .parse()
        .map_err(|_| "CARGO_BUILD_JOBS must be a positive integer")?;
    require(jobs > 0, "CARGO_BUILD_JOBS must be a positive integer")?;
    let workers = jobs.min(8).to_string();
    let mut command = runner.command("./ci/hermetic/run-in-pinned-root.sh");
    command.args([
        "--src",
        ".",
        "--out",
        "ignored/hermetic/split",
        "--src-rw",
        "--cargo-home",
        "ignored/hermetic/split/cargo",
        "--proc-locks-runtime",
    ]);
    for name in PINNED_PHASE_ENV {
        if std::env::var_os(name).is_some() {
            command.args(["--env", name]);
        }
    }
    command
        .args(["--", "bash", "-c", PINNED_PHASE_SHELL, "bash"])
        .arg(pinned_generation.join("request.json"));
    // An actual failing test verdict is not a reason to omit HOST62. Incomplete
    // or mismatched infrastructure handoff is still a fatal error, not a pass.
    let (pinned_status, _) = runner.run("pinned-phase", &mut command)?;
    let handoff = read_json(&generation.join("handoff.json"), clock)?;
    closed_fields(
        &handoff,
        &[
            "schema",
            "request_sha256",
            "generation_relative",
            "started_monotonic_ns",
            "deadline_monotonic_ns",
            "lock_dev",
            "lock_ino",
            "sources_before",
            "sources_after",
            "observed_entry_monotonic_ns",
            "observed_close_monotonic_ns",
            "installation",
            "records",
            "raw",
            "runtime_environment",
        ],
    )?;
    require(
        handoff["schema"] == BRIDGE_SCHEMA
            && handoff["request_sha256"]
                == file_record(
                    &generation.join("request.json"),
                    &generation.join("request.json"),
                    clock,
                )?["sha256"],
        "handoff is not bound to this exact completed request",
    )?;
    for name in [
        "generation_relative",
        "started_monotonic_ns",
        "deadline_monotonic_ns",
        "lock_dev",
        "lock_ino",
    ] {
        require(
            handoff[name] == request[name],
            "handoff changed its generation, lock or original deadline",
        )?;
    }
    let phase_entry = handoff["observed_entry_monotonic_ns"]
        .as_u64()
        .ok_or("phase entry clock missing")?;
    let phase_close = handoff["observed_close_monotonic_ns"]
        .as_u64()
        .ok_or("phase close clock missing")?;
    require(
        clock.started_ns <= phase_entry
            && phase_entry <= phase_close
            && phase_close < clock.deadline_ns
            && phase_close <= monotonic_ns()?,
        "private clock observations differ from the original deadline",
    )?;
    verify_work_lock(work, request, clock)?;
    require(
        handoff["sources_before"] == request["sources"]
            && handoff["sources_after"] == request["sources"]
            && source_records(root, clock)? == request["sources"],
        "source changed across the pinned phase",
    )?;
    verify_bridge(
        root,
        generation,
        &pinned_generation,
        &handoff["records"],
        clock,
    )?;
    let raw = host_raw_paths(root, generation, &pinned_generation, &handoff["raw"])?;
    let records = handoff["records"]
        .as_array()
        .ok_or("production records missing")?;
    let record_paths = records
        .iter()
        .map(|row| text_field(row, "path").map(PathBuf::from))
        .collect::<Result<BTreeSet<_>>>()?;
    for value in handoff["raw"]
        .as_object()
        .ok_or("raw records missing")?
        .values()
    {
        require(
            record_paths.contains(Path::new(value.as_str().ok_or("raw path missing")?)),
            "handoff omitted a mandatory raw record from its retained graph",
        )?;
    }
    for path in [
        "target/validate/hermit",
        "target/validate/test-harness",
        "target/validate/hermit-manifest-plan",
        "target/install_pkg/rsrcs/libdetcore_liteinst.so",
        "target/install_pkg/rsrcs/libdetcore_liteinst.package.json",
    ] {
        require(
            record_paths
                .contains(&Path::new("/src/target/ci/in-guest-trap/no-liteinst/work").join(path)),
            "handoff omitted a required qualified artifact",
        )?;
    }
    closed_fields(
        &handoff["runtime_environment"],
        &[
            "HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE",
            "HERMIT_M2_STACK_FIXTURE_BUNDLE",
            "HERMIT_COLD_STACK_FIXTURE_BUNDLE",
            "REVERIE_LITEINST_PRELOAD",
        ],
    )?;
    for (variable, leaf) in [
        (
            "HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE",
            "m1-allocator-fixtures.path",
        ),
        ("HERMIT_M2_STACK_FIXTURE_BUNDLE", "m2-stack-fixtures.path"),
        (
            "HERMIT_COLD_STACK_FIXTURE_BUNDLE",
            "cold-stack-fixtures.path",
        ),
    ] {
        require(
            Path::new(text_field(&handoff["runtime_environment"], variable)?)
                == Path::new("/src/target/ci/in-guest-trap").join(leaf),
            "pinned fixture environment substituted its maintained pointer",
        )?;
    }
    for (name, value) in handoff["runtime_environment"]
        .as_object()
        .ok_or("runtime environment missing")?
    {
        let logical = Path::new(
            value
                .as_str()
                .ok_or("fixture environment path is not a string")?,
        );
        require(
            record_paths.contains(logical),
            "fixture environment omitted its physical graph identity",
        )?;
        runner.environment.insert(
            name.clone(),
            pinned_to_host(
                root,
                generation,
                &pinned_generation,
                logical,
                &BTreeSet::new(),
            )?,
        );
    }
    let bundle_pointer = &runner.environment["HERMIT_M1_ALLOCATOR_FIXTURE_BUNDLE"];
    let selected_bundle = fs::read_to_string(bundle_pointer).map_err(|e| e.to_string())?;
    let selected_bundle = Path::new(selected_bundle.trim());
    require(
        record_paths.contains(selected_bundle),
        "selected M1 bundle is absent from its retained graph",
    )?;
    let envelope = read_json(
        &pinned_to_host(
            root,
            generation,
            &pinned_generation,
            selected_bundle,
            &BTreeSet::new(),
        )?,
        clock,
    )?;
    require(
        envelope["standalone"]["runtime"]
            == handoff["runtime_environment"]["REVERIE_LITEINST_PRELOAD"],
        "HOST preload differs from the producer's selected standalone fixture",
    )?;
    require(
        records.iter().any(|row| {
            Path::new(row["path"].as_str().unwrap_or(""))
                .file_name()
                .is_some_and(|name| name == "native-qualification.json")
        }),
        "native production qualification is absent",
    )?;
    require(
        installation_records(&installation, clock)? == handoff["installation"],
        "HOST OFF installation differs from the producer tree",
    )?;
    for binary in [&hermit, &harness, &planner] {
        require(
            fs::metadata(binary)
                .map_err(|e| format!("{}: {e}", binary.display()))?
                .mode()
                & 0o111
                != 0,
            "shared pinned-produced binary is not executable on HOST",
        )?;
    }
    let mut command = runner.command("python3");
    command
        .args(["ci/verify-packaged-detcore.py", "--without-liteinst"])
        .arg(&installation);
    runner.checked("host-verify-package", &mut command)?;
    let mut command = runner.command(&hermit);
    command.arg("--version");
    runner.checked("host-pinned-hermit-viability", &mut command)?;
    let host_state = generation.join("host-state");
    fs::create_dir(&host_state).map_err(|e| e.to_string())?;
    runner
        .environment
        .insert("VALIDATE_RUN_STATE".into(), host_state.clone());
    let fixtures = host_state.join("strict-compat/real-compat-fixtures");
    let mut command = runner.command("./tests/compat/prepare_real_compat_fixtures.sh");
    command.arg(&fixtures);
    runner.checked("host-prepare-real-compat-fixtures", &mut command)?;
    let (full_ids, pinned_rows, host_rows) = manifest_partition(&authoritative_rows(root, clock)?)?;
    let pinned_plan = read_json(&raw["manifest_plan"], clock)?;
    let pinned_ids = check_partition_plan(
        pinned_plan
            .as_array()
            .ok_or("pinned plan is not an array")?,
        &pinned_rows,
    )?;
    let mut command = runner.command(&harness);
    command.args([
        "plan",
        "--backend",
        "in-guest-trap",
        "--ci-only",
        "--category",
        "compat",
        "--format",
        "json",
    ]);
    let plan = runner.checked("host-manifest-plan", &mut command)?;
    let plan = read_json(&plan, clock)?;
    let host_ids = check_partition_plan(
        plan.as_array().ok_or("HOST plan is not an array")?,
        &host_rows,
    )?;
    let mut command = runner.command(&harness);
    command.args([
        "build",
        "--backend",
        "in-guest-trap",
        "--ci-only",
        "--category",
        "compat",
        "--jobs",
        &workers,
    ]);
    runner.checked("host-build-manifest-programs", &mut command)?;
    let host_report_path = generation.join("e2e-results-host.json");
    let mut command = runner.command(&harness);
    command
        .args([
            "run",
            "--backend",
            "in-guest-trap",
            "--ci-only",
            "--category",
            "compat",
            "--prebuilt",
            "--jobs",
            &workers,
            "--results",
        ])
        .arg(generation.join("cells-host.jsonl"))
        .env("DAGRUN_TEST_COUNTS_PATH", &host_report_path);
    let (host_status, _) = runner.run("host-manifest-run", &mut command)?;
    let pinned_report = read_report(&raw["manifest_report"])?;
    let host_report = read_report(&host_report_path)?;
    let pinned_population = check_report_population("pinned manifest", &pinned_report, &pinned_ids);
    let host_population = check_report_population("HOST manifest", &host_report, &host_ids);
    require(
        pinned_report.filtered_tests == 0 && host_report.filtered_tests == 0,
        "manifest partition unexpectedly filtered terminal cells",
    )?;
    let manifest_report = merge_reports([pinned_report, host_report])?;
    publish_json(
        &generation.join("e2e-results.json"),
        &serde_json::from_slice(&manifest_report.to_current_json()?).map_err(|e| e.to_string())?,
        clock,
    )?;
    let nextest_ids =
        check_inventory(&fs::read(&raw["nextest_inventory"]).map_err(|e| e.to_string())?)?;
    let isolation_ids = isolation_test_ids(
        &fs::read_to_string(&raw["isolation_inventory"]).map_err(|e| e.to_string())?,
    )?;
    let mixed_ids =
        mixed_runtime_inventory(&fs::read(&raw["mixed_inventory"]).map_err(|e| e.to_string())?)?;
    let nextest_report = read_report(&raw["nextest_report"])?;
    let isolation_report = read_report(&raw["isolation_report"])?;
    let mixed_report = read_report(&raw["mixed_report"])?;
    let nextest_population = check_report_population("Nextest", &nextest_report, &nextest_ids);
    let manifest_population = check_report_population("manifest", &manifest_report, &full_ids);
    let isolation_population =
        check_report_population("isolation", &isolation_report, &isolation_ids);
    let mixed_population = check_report_population("mixed ON/OFF", &mixed_report, &mixed_ids);
    let mut actual_statuses = Vec::new();
    for name in [
        "nextest_status",
        "manifest_status",
        "isolation_status",
        "mixed_status",
    ] {
        let status = read_json(&raw[name], clock)?;
        let success = status["success"]
            .as_bool()
            .ok_or("actual runner success field missing")?;
        require(
            success == (status["exit_code"].as_i64() == Some(0)),
            "actual runner status is inconsistent",
        )?;
        actual_statuses.push(success);
    }
    // Recheck every mandatory physical edge and the complete installation after
    // consumption, before publication and before releasing the same flock.
    verify_work_lock(work, request, clock)?;
    verify_bridge(
        root,
        generation,
        &pinned_generation,
        &handoff["records"],
        clock,
    )?;
    require(
        installation_records(&installation, clock)? == handoff["installation"],
        "HOST consumption changed the OFF installation",
    )?;
    require(
        source_records(root, clock)? == request["sources"],
        "source changed during HOST consumption",
    )?;
    publish_json(
        &generation.join("host-fixtures.json"),
        &installation_records(&fixtures, clock)?,
        clock,
    )?;
    publish_json(
        &generation.join("source-close.json"),
        &source_records(root, clock)?,
        clock,
    )?;
    let report = merge_reports([
        nextest_report,
        manifest_report,
        isolation_report,
        mixed_report,
    ])?;
    let mut command = runner.command(writer);
    command
        .arg(report.executed_tests.to_string())
        .arg(report.filtered_tests.to_string());
    for row in report
        .results
        .as_ref()
        .expect("current report contains rows")
    {
        command
            .arg(&row.id)
            .arg(if row.passed { "pass" } else { "fail" })
            .arg(row.attempts.to_string());
    }
    runner.checked("publish-results", &mut command)?;
    let final_sources = source_records(root, clock)?;
    require(
        final_sources == request["sources"],
        "source changed during structured publication",
    )?;
    publish_json(
        &generation.join("source-post-publication.json"),
        &final_sources,
        clock,
    )?;
    require_test_success(
        [
            actual_statuses[0],
            actual_statuses[1] && host_status.success(),
            actual_statuses[2],
            actual_statuses[3],
            pinned_status.success(),
        ],
        &report,
    )?;
    pinned_population?;
    host_population?;
    nextest_population?;
    manifest_population?;
    isolation_population?;
    mixed_population?;
    clock.check()?;
    println!(
        "in-guest-trap OFF cohorts plus mixed ON/OFF control: {} tests; retained evidence {}",
        report.executed_tests,
        generation.display()
    );
    Ok(())
}

fn run(writer: &Path) -> Result<()> {
    let started = Instant::now();
    let clock = SharedDeadline::start()?;
    let result = (|| -> Result<()> {
        let root = fs::canonicalize(".").map_err(|e| e.to_string())?;
        require(
            fs::canonicalize(writer).map_err(|e| e.to_string())?
                == root.join("ci/write-structured-test-counts.sh"),
            "--results-writer must name the repository's structured result writer",
        )?;
        clock.check()?;
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let relative = PathBuf::from(format!(
            "ignored/in-guest-trap-no-liteinst/{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("ignored/in-guest-trap-no-liteinst"))
            .map_err(|e| e.to_string())?;
        let generation = root.join(&relative);
        fs::create_dir(&generation).map_err(|e| e.to_string())?;
        require(
            fs::canonicalize(&generation).map_err(|e| e.to_string())? == generation,
            "generation escaped through a symlink",
        )?;
        let work = root.join("ignored/hermetic/split/target/ci/in-guest-trap/no-liteinst/work");
        fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        require(
            fs::canonicalize(&work).map_err(|e| e.to_string())? == work,
            "work root escaped through a symlink",
        )?;
        clock.check()?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(work.join("lock"))
            .map_err(|e| e.to_string())?;
        require(
            lock.metadata().map_err(|e| e.to_string())?.is_file(),
            "work lock is not a regular file",
        )?;
        // SAFETY: this exact never-unlinked inode stays owned until all reports,
        // source/artifact closes and publication have completed.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "OFF work directory is already owned or cannot be locked: {}",
                std::io::Error::last_os_error()
            ));
        }
        let metadata = lock.metadata().map_err(|e| e.to_string())?;
        let target = work.join("target");
        let _work = WorkDirectory {
            _lock: lock,
            started,
            evidence: generation.clone(),
            reuse: json!({
                "work_directory":work,"cargo_target_existed":target.is_dir(),
                "standalone_target_existed":work.join("standalone-target").is_dir(),
                "mixed_on_target_existed":work.join("mixed-on-target").is_dir(),"programs_existed":work.join("programs").is_dir(),
            }),
        };
        let request = json!({"schema":BRIDGE_SCHEMA,"generation_relative":relative,
            "started_monotonic_ns":clock.started_ns,"deadline_monotonic_ns":clock.deadline_ns,
            "lock_dev":metadata.dev(),"lock_ino":metadata.ino(),"sources":source_records(&root,&clock)?});
        publish_json(&generation.join("request.json"), &request, &clock)?;
        publish_json(
            &generation.join("host-clock.json"),
            &json!({
            "started_monotonic_ns":clock.started_ns,"deadline_monotonic_ns":clock.deadline_ns,
            "observed_before_launch_ns":monotonic_ns()?}),
            &clock,
        )?;
        run_host_locked(&root, &generation, &work, &request, &clock, writer)
    })();
    // WorkDirectory::drop (including its resource write and unlock) has run.
    // Preserve the original error if it also exhausted the shared deadline.
    match result {
        Err(error) => Err(error),
        Ok(()) => clock.check(),
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments == ["--help"] || arguments == ["-h"] {
        println!(
            "usage: test-in-guest-trap-no-liteinst.rs --results-writer ci/write-structured-test-counts.sh\nRun the official trap-only validation cohort with one shared deadline and lock.\nPrivate --pinned-phase REQUEST is one implementation phase, not a new validation budget."
        );
        return ExitCode::SUCCESS;
    }
    let result = match arguments.as_slice() {
        [option, writer] if option == "--results-writer" => run(Path::new(writer)),
        [option, request] if option == "--pinned-phase" => run_pinned_phase(Path::new(request)),
        _ => Err("expected --results-writer ci/write-structured-test-counts.sh; use --help".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("in-guest-trap without liteinst: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use dagrun::TestResult;

    use super::*;

    fn report(id: &str, passed: bool, attempts: u64) -> TestResults {
        TestResults::current(
            1,
            3,
            vec![TestResult::new(id.into(), passed, attempts).unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn merged_children_keep_failed_verdicts_and_retry_counts() {
        let merged = merge_reports([
            report("cli$case", true, 1),
            report("e2e/case", false, 2),
            report("isolation/case", true, 3),
        ])
        .unwrap();
        assert_eq!(merged.executed_tests, 3);
        assert_eq!(merged.filtered_tests, 9);
        assert_eq!(
            merged.results.unwrap(),
            vec![
                TestResult::new("cli$case".into(), true, 1).unwrap(),
                TestResult::new("e2e/case".into(), false, 2).unwrap(),
                TestResult::new("isolation/case".into(), true, 3).unwrap(),
            ]
        );
    }

    #[test]
    fn empty_and_duplicate_populations_are_refused() {
        for index in 0..3 {
            let mut reports = [
                report("cli$case", true, 1),
                report("e2e/case", true, 1),
                report("isolation/case", true, 1),
            ];
            reports[index] = TestResults::current(0, 0, vec![]).unwrap();
            assert!(merge_reports(reports).is_err());
        }
        assert!(
            merge_reports([
                report("case", true, 1),
                report("case", true, 1),
                report("isolation/case", true, 1),
            ])
            .is_err()
        );
    }

    #[test]
    fn passing_terminal_rows_cannot_hide_a_failed_runner() {
        let passing = merge_reports([
            report("cli$case", true, 1),
            report("e2e/case", true, 1),
            report("isolation/case", true, 1),
        ])
        .unwrap();
        assert!(require_test_success([true; 3], &passing).is_ok());
        for index in 0..3 {
            let mut runners = [true; 3];
            runners[index] = false;
            assert!(require_test_success(runners, &passing).is_err());
        }
        let failing = merge_reports([
            report("cli$case", false, 1),
            report("e2e/case", true, 1),
            report("isolation/case", true, 1),
        ])
        .unwrap();
        assert!(require_test_success([true; 3], &failing).is_err());
    }

    #[test]
    fn mixed_on_control_keeps_its_own_verdict_identity_and_runner_status() {
        let merged = merge_reports([
            report("cli$case", true, 1),
            report("e2e/case", true, 1),
            report("isolation/case", true, 1),
            report("mixed-on$case", false, 2),
        ])
        .unwrap();
        assert_eq!(merged.executed_tests, 4);
        assert_eq!(merged.results.as_ref().unwrap()[3].id, "mixed-on$case");
        assert_eq!(merged.results.as_ref().unwrap()[3].attempts, 2);
        assert!(require_test_success([true; 4], &merged).is_err());
        let passing = merge_reports([
            report("cli$case", true, 1),
            report("e2e/case", true, 1),
            report("isolation/case", true, 1),
            report("mixed-on$case", true, 1),
        ])
        .unwrap();
        assert!(require_test_success([true, true, true, false], &passing).is_err());
        assert!(
            merge_reports([
                report("cli$case", true, 1),
                report("e2e/case", true, 1),
                report("isolation/case", true, 1),
                TestResults::current(0, 0, vec![]).unwrap(),
            ])
            .is_err()
        );
    }

    #[test]
    fn mixed_on_inventory_refuses_substitution_extra_cases_and_ignored_controls() {
        let suite = "hermit-run::liteinst_mixed_runtime";
        let inventory = json!({"rust-suites": {(suite): {
            "package-name": "hermit-run", "binary-name": "liteinst_mixed_runtime",
            "binary-id": suite, "kind": "test", "status": "listed",
            "testcases": {(MIXED_RUNTIME_CASE): {
                "ignored": false, "filter-match": {"status": "matches"}
            }}
        }}});
        let check = |value: &Value| mixed_runtime_inventory(&serde_json::to_vec(value).unwrap());
        assert_eq!(
            check(&inventory).unwrap(),
            BTreeSet::from([format!("{suite}${MIXED_RUNTIME_CASE}")])
        );
        for (field, value) in [
            ("package-name", "another-package"),
            ("binary-name", "cli"),
            ("binary-id", "hermit-run::cli"),
            ("kind", "lib"),
            ("status", "skipped"),
        ] {
            let mut changed = inventory.clone();
            changed["rust-suites"][suite][field] = json!(value);
            assert!(check(&changed).is_err());
        }
        let mut ignored = inventory.clone();
        ignored["rust-suites"][suite]["testcases"][MIXED_RUNTIME_CASE]["ignored"] = json!(true);
        assert!(check(&ignored).is_err());
        let mut extra = inventory.clone();
        extra["rust-suites"][suite]["testcases"]["extra"] =
            inventory["rust-suites"][suite]["testcases"][MIXED_RUNTIME_CASE].clone();
        assert!(check(&extra).is_err());
        assert!(check(&json!({"rust-suites": {}})).is_err());
    }

    #[test]
    fn required_populations_refuse_missing_or_substituted_identities() {
        let report = report("e2e/case", true, 2);
        assert!(
            check_report_population("manifest", &report, &BTreeSet::from(["e2e/case".into()]))
                .is_ok()
        );
        for expected in [
            BTreeSet::new(),
            BTreeSet::from(["e2e/another-case".into()]),
            BTreeSet::from(["e2e/case".into(), "e2e/missing".into()]),
        ] {
            assert!(check_report_population("manifest", &report, &expected).is_err());
        }
        let expected: Value =
            serde_json::from_str(include_str!("../ci/expected-e2e-plan.json")).unwrap();
        let rows = expected["cells"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["backend"] == "in-guest-trap")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(manifest_test_ids(&rows).unwrap().len(), MANIFEST_TEST_COUNT);
        assert!(manifest_test_ids(&rows[1..]).is_err());
        let mut duplicate = rows;
        duplicate[0] = duplicate[1].clone();
        assert!(manifest_test_ids(&duplicate).is_err());
    }

    #[test]
    fn tree_refuses_the_actual_patching_package() {
        assert!(graph_without_patching("hermit-run v0.5.1\nreverie-loader v0.5.1\n").is_ok());
        assert!(graph_without_patching("hermit-run v0.5.1\nliteinst2 v0.1.0\n").is_err());
        assert!(graph_without_patching("").is_err());
    }

    #[test]
    fn portable_exclusions_keep_the_existing_policy_except_four_dead_liteinst_names() {
        let dag: Value = serde_json::from_str(include_str!("../ci/dag/validate.json")).unwrap();
        let cli = dag["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["group"] == "test" && step["job"] == "cli")
            .unwrap();
        let dead_names = [
            "run_liteinst_rejects_non_fork_clone",
            "run_liteinst_handles_inherited_ignored_sigchld",
            "run_liteinst_verifies_forked_guest",
            "run_liteinst_verifies_raw_fork_guest",
        ];
        // These four exclusions have no definitions in the baseline or current
        // CLI sources. The OFF producer must not inherit a dormant exclusion
        // that could silently hide a shared test if an old name is restored.
        let command = cli["cmd"].as_str().unwrap();
        let policy = command
            .rsplit_once("--test cli -- ")
            .unwrap()
            .1
            .strip_suffix('\'')
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>();
        let mut expected = Vec::new();
        let mut removed = BTreeSet::new();
        let (pairs, remainder) = policy.as_chunks::<2>();
        assert!(remainder.is_empty());
        for pair in pairs {
            assert_eq!(pair[0], "--skip");
            if dead_names.contains(&pair[1]) {
                removed.insert(pair[1]);
            } else {
                expected.push(pair[1]);
            }
        }
        assert_eq!(removed, dead_names.into_iter().collect());
        assert_eq!(PORTABLE_SKIPS, expected);
        let arguments = nextest_arguments();
        assert!(
            !arguments
                .iter()
                .any(|arg| arg == "third-party-backends" || arg == "--all-features")
        );
        assert!(arguments.iter().any(|arg| arg == "--no-default-features"));
    }

    const TEST_NEXTEST_POPULATIONS: &[(SuiteKey<'static>, InventoryCounts)] = &[
        (
            ("hermit-run", "lib", "hermit"),
            InventoryCounts::new(1, 0, 0, 1),
        ),
        (
            ("hermit-run", "bin", "hermit"),
            InventoryCounts::new(2, 0, 0, 2),
        ),
        (
            ("hermit-run", "test", "cli"),
            InventoryCounts::new(7, 1, 1, 9),
        ),
        (
            ("hermit-run", "test", "liteinst_allocator"),
            InventoryCounts::new(4, 0, 0, 4),
        ),
        (
            ("hermit-run", "test", "liteinst_stacks"),
            InventoryCounts::new(9, 0, 0, 9),
        ),
        (
            ("hermit-run", "test", "proc_fd_link_determinism"),
            InventoryCounts::new(1, 0, 0, 1),
        ),
        (
            ("hermit-run", "test", "socket_timestamp_determinism"),
            InventoryCounts::new(1, 0, 0, 1),
        ),
        (
            ("hermit-run", "test", "socket_ioctl_timestamp_determinism"),
            InventoryCounts::new(1, 0, 0, 1),
        ),
    ];

    fn check_test_inventory(bytes: &[u8]) -> Result<BTreeSet<String>> {
        check_inventory_with_populations(bytes, TEST_NEXTEST_POPULATIONS)
    }

    fn selected_test_inventory() -> Value {
        let selected = json!({"ignored": false, "filter-match": {"status": "matches"}});
        let mut suites = serde_json::Map::new();
        for (binary, names) in [
            ("liteinst_allocator", ALLOCATOR_TESTS),
            ("liteinst_stacks", STACK_TESTS),
        ] {
            let cases = names
                .iter()
                .map(|name| ((*name).into(), selected.clone()))
                .collect::<serde_json::Map<_, _>>();
            suites.insert(
                binary.into(),
                json!({"package-name": "hermit-run", "binary-name": binary, "kind": "test", "testcases": cases}),
            );
        }
        let names = [
            "in_guest_trap_sigalrm_scalar_identity_matches_ptrace",
            "in_guest_trap_dispatch_record_reports_no_patched_sites",
            "in_guest_trap_verifies_a_program_with_no_site_patched",
            "in_guest_trap_sets_site_patching_off_and_refuses_a_caller_who_turns_it_on",
            "in_guest_trap_keeps_site_patching_off_whatever_a_guest_library_does",
            "liteinst_in_guest_programs::liteinst_in_guest_fork_runs_without_hanging",
            "portable_generic_program",
        ];
        let mut cases = names
            .into_iter()
            .map(|name| (name.into(), selected.clone()))
            .collect::<serde_json::Map<_, _>>();
        cases.insert(
            "ignored_case".into(),
            json!({"ignored": true, "filter-match": {"status": "mismatch", "reason": "ignored"}}),
        );
        cases.insert(
            "filtered_case".into(),
            json!({"ignored": false, "filter-match": {"status": "mismatch", "reason": "string"}}),
        );
        suites.insert(
            "cli".into(),
            json!({"package-name": "hermit-run", "binary-name": "cli", "kind": "test", "testcases": cases}),
        );
        for (binary, name) in SHARED_IN_GUEST_CASES {
            suites.insert(binary.into(), json!({"package-name": "hermit-run", "binary-name": binary, "kind": "test", "testcases": {
                name: selected.clone(),
            }}));
        }
        suites.insert("hermit".into(), json!({"package-name": "hermit-run", "binary-name": "hermit", "kind": "bin", "testcases": {
            "native_exec_observer::tests::actual_observer_keeps_sequence_and_early_close_failure": selected.clone(),
            "overlapping_name": selected.clone(),
        }}));
        suites.insert("hermit-lib".into(), json!({"package-name": "hermit-run", "binary-name": "hermit", "kind": "lib", "testcases": {
            "overlapping_name": selected,
        }}));
        let suites = suites
            .into_values()
            .map(|mut suite| {
                let package = suite["package-name"].as_str().unwrap();
                let binary = suite["binary-name"].as_str().unwrap();
                let id = match suite["kind"].as_str().unwrap() {
                    "lib" => package.to_owned(),
                    "bin" => format!("{package}::bin/{binary}"),
                    "test" => format!("{package}::{binary}"),
                    _ => unreachable!(),
                };
                suite["binary-id"] = json!(id);
                suite["status"] = json!("listed");
                (id, suite)
            })
            .collect::<serde_json::Map<_, _>>();
        let total: usize = suites
            .values()
            .map(|suite| suite["testcases"].as_object().unwrap().len())
            .sum();
        json!({"rust-suites": suites, "test-count": total})
    }

    #[test]
    fn inventory_refuses_a_filtered_shared_trap_test() {
        let inventory = selected_test_inventory();
        assert!(check_test_inventory(&serde_json::to_vec(&inventory).unwrap()).is_ok());
        for (suite, name) in [
            ("cli", "in_guest_trap_sigalrm_scalar_identity_matches_ptrace"),
            (
                "cli",
                "liteinst_in_guest_programs::liteinst_in_guest_fork_runs_without_hanging",
            ),
            (
                "hermit",
                "native_exec_observer::tests::actual_observer_keeps_sequence_and_early_close_failure",
            ),
        ]
        .into_iter()
        .chain(SHARED_IN_GUEST_CASES)
        {
            let mut filtered = inventory.clone();
            // This is the actual JSON form emitted by cargo nextest list for
            // a libtest --skip argument, including the string-filter reason.
            let suite = if suite == "hermit" { "hermit-run::bin/hermit".to_owned() } else { format!("hermit-run::{suite}") };
            filtered["rust-suites"][suite]["testcases"][name]["filter-match"] =
                json!({"status": "mismatch", "reason": "string"});
            let error = check_test_inventory(&serde_json::to_vec(&filtered).unwrap()).unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn inventory_refuses_legacy_positives_substituted_for_off_refusal_controls() {
        for (binary, refusal, positive) in [
            (
                "liteinst_allocator",
                "standalone_legacy_tools_refuse_before_allocator_child_configuration",
                "genuine_standalone_leaf_keeps_scope_free_work_private",
            ),
            (
                "liteinst_stacks",
                "standalone_legacy_tools_refuse_before_altstack_enabled_child_configuration",
                "genuine_standalone_registers_fixed_guarded_altstack_without_continuation",
            ),
            (
                "liteinst_stacks",
                "standalone_legacy_tools_refuse_before_altstack_disabled_child_configuration",
                "standalone_altstack_disabled_receives_no_owned_stack_credit",
            ),
        ] {
            let mut inventory = selected_test_inventory();
            let cases = inventory["rust-suites"][format!("hermit-run::{binary}")]["testcases"]
                .as_object_mut()
                .unwrap();
            let selected = cases.remove(refusal).unwrap();
            assert!(cases.insert(positive.into(), selected).is_none());
            let error = check_test_inventory(&serde_json::to_vec(&inventory).unwrap()).unwrap_err();
            assert!(error.contains("exact OFF scope"), "{error}");
        }
    }

    #[test]
    fn nextest_terminals_bind_every_selected_identity_and_keep_library_and_binary_distinct() {
        let inventory = selected_test_inventory();
        let selected = check_test_inventory(&serde_json::to_vec(&inventory).unwrap()).unwrap();
        assert!(selected.contains("hermit-run$overlapping_name"));
        assert!(selected.contains("hermit-run::hermit$overlapping_name"));
        let rows = selected
            .iter()
            .map(|id| TestResult::new(id.clone(), true, 1).unwrap())
            .collect::<Vec<_>>();
        let complete = TestResults::current(rows.len() as u64, 0, rows.clone()).unwrap();
        assert!(check_report_population("Nextest", &complete, &selected).is_ok());
        let mut substituted = rows;
        substituted[0] =
            TestResult::new("hermit-run::cli$unselected_test".into(), true, 1).unwrap();
        let wrong = TestResults::current(substituted.len() as u64, 0, substituted).unwrap();
        assert!(check_report_population("Nextest", &wrong, &selected).is_err());
        let missing = TestResults::current(
            1,
            0,
            vec![TestResult::new("hermit-run$overlapping_name".into(), true, 1).unwrap()],
        )
        .unwrap();
        assert!(check_report_population("Nextest", &missing, &selected).is_err());
    }

    #[test]
    fn inventory_refuses_missing_empty_duplicate_or_unlisted_suites() {
        let inventory = selected_test_inventory();
        assert!(check_test_inventory(&serde_json::to_vec(&inventory).unwrap()).is_ok());
        for key in ["hermit-run", "hermit-run::bin/hermit"] {
            let mut missing = inventory.clone();
            let removed = missing["rust-suites"]
                .as_object_mut()
                .unwrap()
                .remove(key)
                .unwrap();
            // Preserve a self-consistent scalar: the exact suite map must
            // independently refuse an omitted library or binary population.
            missing["test-count"] = json!(
                inventory["test-count"].as_u64().unwrap()
                    - removed["testcases"].as_object().unwrap().len() as u64
            );
            let error = check_test_inventory(&serde_json::to_vec(&missing).unwrap()).unwrap_err();
            assert!(error.contains("suite populations"), "{error}");
        }
        let mut extra = inventory.clone();
        extra["rust-suites"]["hermit-run::unknown"] = json!({
            "package-name": "hermit-run", "binary-name": "unknown", "kind": "test", "binary-id": "hermit-run::unknown", "status": "listed", "testcases": {},
        });
        assert!(check_test_inventory(&serde_json::to_vec(&extra).unwrap()).is_err());
        let mut duplicate = inventory.clone();
        duplicate["rust-suites"]["duplicate"] = duplicate["rust-suites"]["hermit-run"].clone();
        assert!(check_test_inventory(&serde_json::to_vec(&duplicate).unwrap()).is_err());
        let mut substituted_kind = inventory.clone();
        substituted_kind["rust-suites"]["hermit-run"]["kind"] = json!("bin");
        assert!(check_test_inventory(&serde_json::to_vec(&substituted_kind).unwrap()).is_err());
        for field in ["status", "binary-id", "kind"] {
            let mut malformed = inventory.clone();
            malformed["rust-suites"]["hermit-run"][field] = json!("wrong");
            assert!(check_test_inventory(&serde_json::to_vec(&malformed).unwrap()).is_err());
        }
    }

    #[test]
    fn inventory_refuses_filtered_generic_cases_or_changed_ignored_population() {
        let inventory = selected_test_inventory();
        for (suite, name) in [
            ("hermit-run", "overlapping_name"),
            ("hermit-run::bin/hermit", "overlapping_name"),
            ("hermit-run::cli", "portable_generic_program"),
        ] {
            let mut filtered = inventory.clone();
            filtered["rust-suites"][suite]["testcases"][name]["filter-match"] =
                json!({"status": "mismatch", "reason": "string"});
            assert!(check_test_inventory(&serde_json::to_vec(&filtered).unwrap()).is_err());
            let mut ignored = inventory.clone();
            ignored["rust-suites"][suite]["testcases"][name]["ignored"] = json!(true);
            assert!(check_test_inventory(&serde_json::to_vec(&ignored).unwrap()).is_err());
        }
        for count in [json!(27), json!(29), json!("28"), Value::Null] {
            let mut wrong_count = inventory.clone();
            wrong_count["test-count"] = count;
            assert!(check_test_inventory(&serde_json::to_vec(&wrong_count).unwrap()).is_err());
        }
        // Synthetic populations cannot satisfy the fixed production contract.
        assert!(super::check_inventory(&serde_json::to_vec(&inventory).unwrap()).is_err());
    }
    fn required_manifest_rows() -> Vec<Value> {
        let value: Value =
            serde_json::from_str(include_str!("../ci/expected-e2e-plan.json")).unwrap();
        value["cells"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["backend"] == "in-guest-trap")
            .cloned()
            .collect()
    }

    #[test]
    fn host_and_pinned_manifest_partitions_preserve_the_complete_required_census() {
        let rows = required_manifest_rows();
        let (full, pinned, host) = manifest_partition(&rows).unwrap();
        assert_eq!(full.len(), 303);
        assert_eq!(host.len(), 62);
        assert_eq!(pinned.len(), 241);
        assert!(host.iter().all(|row| row["category"] == "compat"));
        assert!(pinned.iter().all(|row| row["category"] != "compat"));
        let host_ids = check_partition_plan(&host, &host).unwrap();
        let pinned_ids = check_partition_plan(&pinned, &pinned).unwrap();
        assert!(host_ids.is_disjoint(&pinned_ids));
        assert_eq!(
            host_ids
                .union(&pinned_ids)
                .cloned()
                .collect::<BTreeSet<_>>(),
            full
        );
        let mut duplicate = rows.clone();
        duplicate[1] = duplicate[0].clone();
        assert!(manifest_partition(&duplicate).is_err());
        let mut recategorized = rows;
        let compat = recategorized
            .iter_mut()
            .find(|row| row["category"] == "compat")
            .unwrap();
        compat["category"] = json!("other");
        assert!(manifest_partition(&recategorized).is_err());
    }

    #[test]
    fn partition_plans_refuse_missing_duplicate_substituted_and_foreign_cells() {
        let (_, _, host) = manifest_partition(&required_manifest_rows()).unwrap();
        assert!(check_partition_plan(&host[..host.len() - 1], &host).is_err());
        for changed in ["duplicate", "substituted", "backend", "mode"] {
            let mut actual = host.clone();
            match changed {
                "duplicate" => actual[1] = actual[0].clone(),
                "substituted" => actual[0]["test"] = json!("unrelated_cell"),
                "backend" => actual[0]["backend"] = json!("ptrace"),
                "mode" => actual[0]["mode"] = json!("unrequested-mode"),
                _ => unreachable!(),
            }
            assert!(
                check_partition_plan(&actual, &host).is_err(),
                "accepted {changed}"
            );
        }
    }

    #[test]
    fn shared_deadline_rejects_an_expired_clock_or_a_new_phase_allowance() {
        let now = monotonic_ns().unwrap();
        assert!(
            SharedDeadline::from_epoch(now, now + (DEADLINE_SECONDS + 1) * 1_000_000_000).is_err()
        );
        assert!(SharedDeadline::from_epoch(u64::MAX, u64::MAX).is_err());
        let expired = SharedDeadline {
            started_ns: now.saturating_sub(1),
            deadline_ns: now.saturating_sub(1),
            instant: Instant::now() + Duration::from_secs(1),
        };
        assert!(expired.check().is_err());
        let request = json!({"schema":BRIDGE_SCHEMA});
        assert!(closed_fields(&request, &["schema", "deadline_monotonic_ns"]).is_err());
    }

    struct BridgeTestDirectory(PathBuf);
    impl Drop for BridgeTestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bridge_rejects_changed_bytes_symlink_escapes_and_incomplete_publication() {
        let serial = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("hermit-off-bridge-{}-{serial}", std::process::id()));
        fs::create_dir(&base).unwrap();
        let _owned = BridgeTestDirectory(base.clone());
        let generation = base.join("generation");
        fs::create_dir(&generation).unwrap();
        let pinned = Path::new("/src/ignored/in-guest-trap-no-liteinst/unit");
        let actual = generation.join("record.json");
        fs::write(&actual, b"{\"actual\":1}").unwrap();
        let now = monotonic_ns().unwrap();
        let clock =
            SharedDeadline::from_epoch(now, now + DEADLINE_SECONDS * 1_000_000_000).unwrap();
        let logical = pinned.join("record.json");
        let record = file_record(&actual, &logical, &clock).unwrap();
        let copies = generation.join("proof-blobs");
        fs::create_dir(&copies).unwrap();
        fs::copy(&actual, copies.join(record["sha256"].as_str().unwrap())).unwrap();
        let records = json!([record]);
        verify_bridge(&base, &generation, pinned, &records, &clock).unwrap();
        fs::write(&actual, b"{\"actual\":2}").unwrap();
        assert!(verify_bridge(&base, &generation, pinned, &records, &clock).is_err());
        assert!(
            pinned_to_host(
                &base,
                &generation,
                pinned,
                Path::new("/src/target/../outside"),
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            pinned_to_host(
                &base,
                &generation,
                pinned,
                Path::new("/nix/store/../../outside"),
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            pinned_to_host(
                &base,
                &generation,
                pinned,
                Path::new("/unmapped/current-proof"),
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            pinned_to_host(
                &base,
                &generation,
                pinned,
                Path::new("/build/.cargo/config.toml"),
                &BTreeSet::new()
            )
            .is_err()
        );
        for cache in ["git", "registry"] {
            let directory = base.join("ignored/hermetic/split/cargo").join(cache);
            fs::create_dir_all(&directory).unwrap();
            let actual = directory.join("source.rs");
            fs::write(&actual, b"actual cached input").unwrap();
            let logical = Path::new("/build/.cargo").join(cache).join("source.rs");
            assert_eq!(
                pinned_to_host(&base, &generation, pinned, &logical, &BTreeSet::new()).unwrap(),
                actual
            );
            assert!(
                pinned_to_host(
                    &base,
                    &generation,
                    pinned,
                    &logical.join("../outside"),
                    &BTreeSet::new()
                )
                .is_err()
            );
        }
        std::os::unix::fs::symlink(base.join("outside"), generation.join("escape")).unwrap();
        fs::write(base.join("outside"), b"outside").unwrap();
        assert!(
            pinned_to_host(
                &base,
                &generation,
                pinned,
                &pinned.join("escape"),
                &BTreeSet::new()
            )
            .is_err()
        );
        let published = generation.join("completed.json");
        publish_json(&published, &json!({"complete":true}), &clock).unwrap();
        assert_eq!(
            read_json(&published, &clock).unwrap(),
            json!({"complete":true})
        );
        assert!(publish_json(&published, &json!({"complete":false}), &clock).is_err());
        assert_eq!(
            read_json(&published, &clock).unwrap(),
            json!({"complete":true})
        );
        let partial = generation.join("partial.json");
        fs::write(&partial, b"{").unwrap();
        assert!(read_json(&partial, &clock).is_err());
    }

    #[test]
    fn cli_depfile_keeps_actual_native_component_paths_and_refuses_broken_escapes() {
        let paths = depfile_paths("/target/hermit: /src/source.rs /target/out/native_exec_runner.bin /path/with\\ space\n\n/src/source.rs:\n").unwrap();
        assert!(paths.contains(&PathBuf::from("/target/out/native_exec_runner.bin")));
        assert!(paths.contains(&PathBuf::from("/path/with space")));
        assert!(depfile_paths("not a Cargo depfile").is_err());
        assert!(depfile_paths("target: dangling\\").is_err());
    }
}
