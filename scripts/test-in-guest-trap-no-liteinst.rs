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
use std::os::unix::process::CommandExt;
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
        InventoryCounts::new(476, 0, 0, 476),
    ),
    (
        ("hermit-run", "test", "cli"),
        InventoryCounts::new(581, 6, 120, 707),
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
        InventoryCounts::new(611, 2, 0, 613),
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
    require(ids.len() == 1, "mixed-runtime inventory must select exactly one case")?;
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
        if let Err(error) = fs::write(
            self.evidence.join("resources.json"),
            format!("{record}\n"),
        ) {
            eprintln!("cannot retain OFF resource measurement: {error}");
        }
        eprintln!("in-guest-trap OFF resource measurement: {record}");
    }
}

fn run(writer: &Path) -> Result<()> {
    let root = fs::canonicalize(".").map_err(|e| e.to_string())?;
    require(
        fs::canonicalize(writer).map_err(|e| e.to_string())?
            == root.join("ci/write-structured-test-counts.sh"),
        "--results-writer must name the repository's structured result writer",
    )?;
    let serial = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let generation = root.join(format!(
        "ignored/in-guest-trap-no-liteinst/{}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&generation).map_err(|e| e.to_string())?;
    let work = root.join("target/ci/in-guest-trap/no-liteinst/work");
    fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(work.join("lock"))
        .map_err(|e| e.to_string())?;
    // SAFETY: lock owns a live descriptor for the complete script invocation.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!(
            "OFF work directory is already owned or cannot be locked: {}",
            std::io::Error::last_os_error()
        ));
    }
    let target = work.join("target");
    let _work = WorkDirectory {
        _lock: lock,
        started: Instant::now(),
        evidence: generation.clone(),
        reuse: json!({
            "work_directory": work,
            "cargo_target_existed": target.is_dir(),
            "standalone_target_existed": work.join("standalone-target").is_dir(),
            "mixed_on_target_existed": work.join("mixed-on-target").is_dir(),
            "programs_existed": work.join("programs").is_dir(),
        }),
    };
    let installation = target.join("install_pkg");
    let hermit = target.join("validate/hermit");
    let mut runner = Runner {
        root: root.clone(),
        evidence: generation.clone(),
        environment: BTreeMap::from([
            ("CARGO_TARGET_DIR".into(), target.clone()),
            ("HERMIT_INSTALL_DIR".into(), installation.clone()),
            ("HERMIT_BIN".into(), hermit.clone()),
            ("HERMIT_LITEINST_TEST_BINARY".into(), hermit.clone()),
            ("E2E_BUILD_ROOT".into(), work.join("programs")),
            ("E2E_RESULT_ROOT".into(), generation.join("e2e")),
            (
                "E2E_RUN_ID".into(),
                PathBuf::from(format!("trap-only-{}-{serial}", std::process::id())),
            ),
        ]),
        deadline: Instant::now() + Duration::from_secs(DEADLINE_SECONDS),
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
    let nextest_ids = check_inventory(&fs::read(inventory).map_err(|e| e.to_string())?)?;
    let nextest_report = generation.join("nextest-results.json");
    let mut command = runner.command("./ci/run-nextest-counted.sh");
    if std::env::var_os("CI").is_some() {
        command.args(["--profile", "ci"]);
    }
    command
        .args(&args)
        .env("DAGRUN_TEST_COUNTS_PATH", &nextest_report);
    let (nextest_status, _) = runner.run("nextest-run", &mut command)?;
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
    let manifest_ids = manifest_test_ids(&expected_rows)?;
    let mut command = runner.command(&harness);
    command.args([
        "plan",
        "--backend",
        "in-guest-trap",
        "--ci-only",
        "--format",
        "json",
    ]);
    let plan = runner.checked("manifest-plan", &mut command)?;
    let plan: Value = serde_json::from_slice(&fs::read(plan).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    require(
        manifest_test_ids(
            plan.as_array()
                .ok_or("selected manifest plan is not an array")?,
        )? == manifest_ids,
        "trap manifest selection differs from the authoritative required plan",
    )?;
    let mut command = runner.command(&harness);
    command.args([
        "build",
        "--backend",
        "in-guest-trap",
        "--ci-only",
        "--jobs",
        &workers,
    ]);
    runner.checked("build-manifest-programs", &mut command)?;
    let e2e_report = generation.join("e2e-results.json");
    let mut command = runner.command(&harness);
    command
        .args([
            "run",
            "--backend",
            "in-guest-trap",
            "--ci-only",
            "--prebuilt",
            "--jobs",
            &workers,
            "--results",
        ])
        .arg(generation.join("cells.jsonl"))
        .env("DAGRUN_TEST_COUNTS_PATH", &e2e_report);
    let (e2e_status, _) = runner.run("manifest-run", &mut command)?;
    // The isolation matrix is an independent required cohort, outside the E2E
    // manifests. Keep its existing full assertion scope and mismatch ledger.
    let mut command = runner.command("./tests/isolation/conformance.rs");
    command.args(["--backends", "in-guest-trap", "--list"]);
    let inventory = runner.checked("isolation-inventory", &mut command)?;
    let isolation_ids =
        isolation_test_ids(&fs::read_to_string(inventory).map_err(|e| e.to_string())?)?;
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
    let (isolation_status, _) = runner.run("isolation-run", &mut command)?;
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
    let mixed_ids = mixed_runtime_inventory(&fs::read(inventory).map_err(|e| e.to_string())?)?;
    let mut command = runner.command("./ci/run-nextest-counted.sh");
    command
        .args(mixed_arguments)
        .env("CARGO_TARGET_DIR", &mixed_target)
        .env("HERMIT_BIN", &mixed_hermit)
        .env("HERMIT_LITEINST_TEST_BINARY", &mixed_hermit)
        .env("HERMIT_MIXED_LITEINST_OFF_INSTALLATION", &installation)
        .env("NEXTEST_EXPECTED_EXECUTED", "1")
        .env("DAGRUN_TEST_COUNTS_PATH", &mixed_report);
    let (mixed_status, _) = runner.run("mixed-on-run", &mut command)?;
    let nextest_report = read_report(&nextest_report)?;
    let e2e_report = read_report(&e2e_report)?;
    let isolation_report = read_report(&isolation_report)?;
    let mixed_report = read_report(&mixed_report)?;
    let nextest_population = check_report_population("Nextest", &nextest_report, &nextest_ids);
    let manifest_population = check_report_population("manifest", &e2e_report, &manifest_ids);
    let isolation_population =
        check_report_population("isolation", &isolation_report, &isolation_ids);
    let mixed_population = check_report_population("mixed ON/OFF", &mixed_report, &mixed_ids);
    let report = merge_reports([nextest_report, e2e_report, isolation_report, mixed_report])?;
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
    require_test_success(
        [
            nextest_status.success(),
            e2e_status.success(),
            isolation_status.success(),
            mixed_status.success(),
        ],
        &report,
    )?;
    nextest_population?;
    manifest_population?;
    isolation_population?;
    mixed_population?;
    println!(
        "in-guest-trap OFF cohorts plus mixed ON/OFF control: {} tests; retained evidence {}",
        report.executed_tests,
        generation.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments == ["--help"] || arguments == ["-h"] {
        println!(
            "usage: test-in-guest-trap-no-liteinst.rs --results-writer ci/write-structured-test-counts.sh\nRun the official isolated trap-only validation cohort; keep all command and test evidence."
        );
        return ExitCode::SUCCESS;
    }
    let result = match arguments.as_slice() {
        [option, writer] if option == "--results-writer" => run(Path::new(writer)),
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
        assert!(merge_reports([
            report("cli$case", true, 1),
            report("e2e/case", true, 1),
            report("isolation/case", true, 1),
            TestResults::current(0, 0, vec![]).unwrap(),
        ])
        .is_err());
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
}
