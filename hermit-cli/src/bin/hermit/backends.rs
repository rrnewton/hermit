/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// AUTONOMOUS-BOT-IMPLEMENTED

//! Execution-backend dispatch for `hermit run`.
//!
//! The DBT path launches the real guest through DynamoRIO and links the native
//! client against Hermit's `detcore-dbt` runtime. That runtime instantiates the
//! production [`detcore::Detcore`] Tool over [`reverie_dbt::DbtGuest`].
//!
//! Generic SaBRe runs are coordinated by `libhermit` with the real Detcore
//! plugin. This module retains the separate
//! `hermit --backend sabre strace` diagnostic path.

use std::collections::BTreeMap;
#[cfg(feature = "dbt")]
use std::collections::BTreeSet;
#[cfg(feature = "dbt")]
use std::env;
#[cfg(feature = "dbt")]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
#[cfg(feature = "dbt")]
use std::io::IsTerminal as _;
#[cfg(feature = "dbt")]
use std::io::Read;
#[cfg(feature = "dbt")]
use std::io::Seek as _;
#[cfg(feature = "dbt")]
use std::io::SeekFrom;
#[cfg(feature = "dbt")]
use std::io::Write;
#[cfg(feature = "dbt")]
use std::os::fd::AsRawFd;
#[cfg(feature = "dbt")]
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
#[cfg(any(feature = "dbt", test))]
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
#[cfg(feature = "dbt")]
use std::path::PathBuf;
use std::process::Command as StdCommand;
#[cfg(feature = "dbt")]
use std::process::Output;

use detcore::Config;
use hermit::Error;
use hermit::ExitStatus;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtEvidenceLogLevel;
#[cfg(feature = "dbt")]
use reverie_dbt::DbtRunner;
#[cfg(feature = "dbt")]
use reverie_dbt::backend_stats::DbtBackendStatsAggregator;
#[cfg(feature = "dbt")]
use reverie_dbt::backend_stats::DbtBackendStatsSnapshot;
use tracing::metadata::LevelFilter;

#[cfg(feature = "dbt")]
use super::record_envelope::RecordEnvelope;
use super::run::VerifyAllow;
#[cfg(feature = "dbt")]
use super::verify::ComparedRun;
#[cfg(feature = "dbt")]
use super::verify::ComparisonOptions;
#[cfg(feature = "dbt")]
use super::verify::DbtCountedBranchComparison;
#[cfg(feature = "dbt")]
use super::verify::LogCompareStrictness;
#[cfg(feature = "dbt")]
use super::verify::Verdict;
#[cfg(feature = "dbt")]
use super::verify::VerificationOutcome;
#[cfg(feature = "dbt")]
use super::verify::announce_verification_outcome;
#[cfg(feature = "dbt")]
use super::verify::compare_two_runs;
#[cfg(feature = "dbt")]
use super::verify::retain_verification_logs;
#[cfg(feature = "dbt")]
use super::verify::temp_log_files_in;
#[cfg(feature = "dbt")]
use super::verify::verification_log_level;
#[cfg(feature = "dbt")]
use super::verify::write_pending_verification_json;
#[cfg(feature = "dbt")]
use super::verify::write_verification_json;

#[derive(Debug)]
#[cfg(feature = "dbt")]
struct DbtSummary {
    branches: u64,
    syscalls: u64,
    rewritten: u64,
    stdin_reads: u64,
    memory_hash: String,
}

#[cfg(feature = "dbt")]
const DBT_SUMMARY_PREFIX: &str = "reverie-dbt: tool=Detcore ";

#[cfg(feature = "dbt")]
impl DbtSummary {
    fn same_observable_behavior(&self, other: &Self) -> bool {
        // `branches` is deliberately absent here because the counted-branch
        // clock is checked separately, with its own failure message. It is NOT
        // excluded because a difference is tolerable: the DynamoRIO client
        // advances the clock only at counted application branches, making it a
        // deterministic function of the executed instruction stream.
        self.syscalls == other.syscalls
            && self.rewritten == other.rewritten
            && self.stdin_reads == other.stdin_reads
            && self.memory_hash == other.memory_hash
    }
}

/// Describe a counted-branch-clock divergence without conflating it with the
/// other native summary fields.
#[cfg(feature = "dbt")]
fn dbt_branch_clock_mismatch(first: u64, second: u64) -> Option<String> {
    (first != second).then(|| {
        format!(
            "DBT verification failed: counted-branch clocks differed between runs ({first} != {second}); \
             the clock is a deterministic function of the executed instruction stream"
        )
    })
}

#[cfg(feature = "dbt")]
fn require_matching_dbt_branch_clocks(first: u64, second: u64) -> Result<(), Error> {
    match dbt_branch_clock_mismatch(first, second) {
        Some(message) => Err(Error::msg(message)),
        None => Ok(()),
    }
}

/// Add a backend-observed divergence to the typed verification verdict.
///
/// The canonical comparator may have matched its stdout, stderr, status, and
/// INFO records, or it may have refused a truncated log. Neither can erase a
/// separately observed difference in the deterministic counted-branch clock.
/// Mutating the typed outcome before it is serialized ensures `--verify-json`
/// says `diverged` with `verified=false` and `bitwise_parity=false`, rather than
/// leaving a false match or the invocation's pending `no_result` record.
#[cfg(feature = "dbt")]
fn record_dbt_branch_clock_comparison(
    outcome: &mut VerificationOutcome,
    comparison: DbtCountedBranchComparison,
) -> Option<String> {
    match dbt_branch_clock_mismatch(comparison.left, comparison.right) {
        Some(message) => {
            outcome.dbt_counted_branches = Some(comparison);
            outcome.verdict = Verdict::Diverged;
            Some(message)
        }
        None if outcome.verdict == Verdict::NoResult => {
            // Equal clocks do not turn a refused common comparison into a
            // verdict. Keep the backend field absent so `no_result` does not
            // claim that this one successful dimension authorized anything.
            outcome.dbt_counted_branches = None;
            None
        }
        None => {
            outcome.dbt_counted_branches = Some(comparison);
            None
        }
    }
}

/// Attach the DBT-specific comparison and publish the terminal typed verdict.
///
/// Returning the human-readable failure only after the JSON write preserves the
/// report-first ordering: once a terminal branch-clock failure is announced,
/// the artifact already names the same divergence and both compared values.
#[cfg(feature = "dbt")]
fn finalize_dbt_verification(
    mut outcome: VerificationOutcome,
    comparison: DbtCountedBranchComparison,
    verify_json: Option<&Path>,
) -> Result<(VerificationOutcome, Option<String>), Error> {
    let failure = record_dbt_branch_clock_comparison(&mut outcome, comparison);
    if let Some(path) = verify_json {
        write_verification_json(path, &outcome)?;
    }
    Ok((outcome, failure))
}

/// Own the typed statistics stream for one complete DBT process tree.
///
/// Reverie's client writes one fixed-size record per process image at exit.
/// The protected-evidence runner does not expose its convenience stats method,
/// so this adapter uses the same public typed wire decoder and waits until the
/// runner has reaped the whole isolated process group before reading it.
#[cfg(feature = "dbt")]
struct DbtStatsCapture {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

#[cfg(feature = "dbt")]
impl DbtStatsCapture {
    fn new() -> Result<Self, Error> {
        let directory = tempfile::Builder::new()
            .prefix("hermit-dbt-verify-stats-")
            .tempdir()
            .map_err(|error| {
                Error::msg(format!(
                    "failed to create DBT whole-process statistics sink: {error}"
                ))
            })?;
        let path = directory.path().join("records.bin");
        Ok(Self {
            _directory: directory,
            path,
        })
    }

    fn configure(&self, runner: DbtRunner) -> DbtRunner {
        runner
            .client_argument("-stats_path")
            .client_argument(self.path.clone().into_os_string())
    }

    fn finish(self) -> Result<DbtBackendStatsSnapshot, Error> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            Ok(_) => {
                return Err(Error::msg(
                    "DBT verification did not reach a verdict: typed whole-process statistics were empty",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::msg(
                    "DBT verification did not reach a verdict: typed whole-process statistics were missing",
                ));
            }
            Err(error) => {
                return Err(Error::msg(format!(
                    "DBT verification did not reach a verdict: failed to read typed whole-process statistics: {error}"
                )));
            }
        };
        let mut aggregator = DbtBackendStatsAggregator::new();
        let records = aggregator.absorb_wire_stream(&bytes).map_err(|error| {
            Error::msg(format!(
                "DBT verification did not reach a verdict: typed whole-process statistics were unreadable: {error}"
            ))
        })?;
        if records == 0 {
            return Err(Error::msg(
                "DBT verification did not reach a verdict: typed whole-process statistics contained no process records",
            ));
        }
        Ok(aggregator.snapshot())
    }
}

/// Render the native DBT counters as a labeled `--summary` block.
///
/// These are the counters the DynamoRIO client already emits at exit; hermit
/// simply surfaces them on the normal run path. Labels are deliberately honest
/// about what each number is: `branches` is Detcore's deterministic
/// counted-branch clock (cbr/ubr/call/return retired), **not** a count of
/// translated basic blocks, and `memory hash` is the client's observed
/// guest-memory digest, not a Detcore RunSummary field.
#[cfg(feature = "dbt")]
fn format_dbt_stats(summary: &DbtSummary) -> String {
    format!(
        "=== DBT backend stats (native DynamoRIO client) ===\n\
         counted branches (deterministic branch clock): {}\n\
         syscalls intercepted:                          {}\n\
         syscall instructions rewritten:                {}\n\
         stdin (fd 0) reads:                            {}\n\
         observed guest-memory hash:                    {}\n",
        summary.branches,
        summary.syscalls,
        summary.rewritten,
        summary.stdin_reads,
        summary.memory_hash,
    )
}

#[derive(Debug, Eq, PartialEq)]
#[cfg(feature = "dbt")]
struct DbtGuestCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

#[cfg(feature = "dbt")]
fn executable_on_path(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|directory| directory.join(program))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

/// Resolve the simple `#!/usr/bin/env PROGRAM` form before DynamoRIO starts.
///
/// DynamoRIO follows an absolute exec target correctly, but its copied exec path
/// can wait indefinitely when `env` later resolves a bare target through PATH.
/// Keep `env` in the process chain and replace only its single plain program
/// token with the equivalent absolute PATH match. More complex `env` forms are
/// left unchanged for the normal launcher rather than partially interpreting
/// options or assignments here.
#[cfg(feature = "dbt")]
fn prepare_dbt_guest_command(
    program: &Path,
    args: &[String],
    path: Option<&OsStr>,
) -> DbtGuestCommand {
    let unchanged = || DbtGuestCommand {
        program: program.to_path_buf(),
        args: args.iter().map(OsString::from).collect(),
    };
    let Some(shebang) = hermit::Shebang::new(program) else {
        return unchanged();
    };
    let (interpreter, interpreter_args) = shebang.into_parts();
    if interpreter.file_name() != Some(OsStr::new("env")) || interpreter_args.len() != 1 {
        return unchanged();
    }

    let target = &interpreter_args[0];
    let target_bytes = std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str());
    if target_bytes.starts_with(b"-")
        || target_bytes.contains(&b'=')
        || target_bytes.contains(&b'/')
    {
        return unchanged();
    }
    let Some(target) = path.and_then(|path| executable_on_path(target, path)) else {
        return unchanged();
    };

    let mut resolved_args = Vec::with_capacity(args.len() + 2);
    resolved_args.push(target.into_os_string());
    resolved_args.push(program.as_os_str().to_owned());
    resolved_args.extend(args.iter().map(OsString::from));
    DbtGuestCommand {
        program: interpreter,
        args: resolved_args,
    }
}

#[cfg(feature = "dbt")]
fn apply_exact_environment(command: &mut StdCommand, environment: &BTreeMap<OsString, OsString>) {
    // DbtRunner reconstructs its launcher command from Command::get_envs(),
    // which cannot expose env_clear(). Make removals explicit so --base-env
    // does not accidentally inherit the Hermit launcher's environment.
    for (key, _) in env::vars_os() {
        if !environment.contains_key(&key) {
            command.env_remove(key);
        }
    }
    command.envs(environment);
}
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review inherited DBT policy descriptors and bounded reports.
#[cfg(feature = "dbt")]
struct InstalledFd {
    target: i32,
    backup: Option<i32>,
    original_flags: Option<i32>,
}

#[cfg(feature = "dbt")]
impl InstalledFd {
    fn install(source: i32, target: i32) -> std::io::Result<Self> {
        // Keep the backup above the reserved transport descriptor so installing the target
        // cannot overwrite its backup.
        let backup = unsafe {
            libc::fcntl(
                target,
                libc::F_DUPFD_CLOEXEC,
                detcore_dbt::UNSUPPORTED_SYSCALL_REPORT_FD + 1,
            )
        };
        let backup = if backup == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EBADF) {
                None
            } else {
                return Err(error);
            }
        } else {
            Some(backup)
        };
        let original_flags = if let Some(backup_fd) = backup {
            let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
            if flags == -1 {
                let error = std::io::Error::last_os_error();
                let _ = unsafe { libc::close(backup_fd) };
                return Err(error);
            }
            Some(flags)
        } else {
            None
        };
        let installed = Self {
            target,
            backup,
            original_flags,
        };
        if unsafe { libc::dup2(source, target) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(target, libc::F_SETFD, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            drop(installed);
            return Err(error);
        }
        Ok(installed)
    }
}

#[cfg(feature = "dbt")]
impl Drop for InstalledFd {
    fn drop(&mut self) {
        if let Some(backup) = self.backup {
            let _ = unsafe { libc::dup2(backup, self.target) };
            if let Some(flags) = self.original_flags {
                let _ = unsafe { libc::fcntl(self.target, libc::F_SETFD, flags) };
            }
            let _ = unsafe { libc::close(backup) };
        } else {
            let _ = unsafe { libc::close(self.target) };
        }
    }
}

#[cfg(feature = "dbt")]
struct DbtUnsupportedSyscallReport {
    reader: std::fs::File,
    _writer: std::fs::File,
    _report_fd: InstalledFd,
}

#[cfg(feature = "dbt")]
impl DbtUnsupportedSyscallReport {
    fn new() -> std::io::Result<Self> {
        let mut descriptors = [-1; 2];
        let result =
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: pipe2 initialized both descriptors, transferring their ownership here.
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        let report_fd = InstalledFd::install(
            writer.as_raw_fd(),
            detcore_dbt::UNSUPPORTED_SYSCALL_REPORT_FD,
        )?;
        Ok(Self {
            reader,
            _writer: writer,
            _report_fd: report_fd,
        })
    }

    fn emit(&mut self) -> std::io::Result<()> {
        const MAX_REPORT_BYTES: usize = 1024 * 1024;
        let mut contents = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match self.reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if contents.len() + read > MAX_REPORT_BYTES {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "DBT unsupported-syscall report exceeded 1 MiB",
                        ));
                    }
                    contents.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let contents = String::from_utf8_lossy(&contents);
        let syscalls = contents
            .lines()
            .filter_map(|line| {
                if let Some(raw) = line.strip_prefix("@") {
                    let sysno = raw
                        .parse::<i32>()
                        .ok()
                        .map(reverie::syscalls::Sysno::from)?;
                    detcore::is_unsupported_syscall(sysno).then(|| sysno.to_string())
                } else if !line.is_empty()
                    && line.len() <= 64
                    && line
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    Some(line.to_owned())
                } else {
                    None
                }
            })
            .take(512)
            .collect::<BTreeSet<_>>();
        if let Some(message) = detcore::format_unsupported_syscall_warning(&syscalls) {
            eprintln!("WARNING: {message}");
        }
        Ok(())
    }
}

#[cfg(feature = "dbt")]
impl Drop for DbtUnsupportedSyscallReport {
    fn drop(&mut self) {
        if let Err(error) = self.emit() {
            eprintln!("WARNING: failed to read DBT unsupported-syscall report: {error}");
        }
    }
}

#[cfg(feature = "dbt")]
struct TeeReader<R, W> {
    input: R,
    replay: W,
}

#[cfg(feature = "dbt")]
impl<R: Read, W: Write> Read for TeeReader<R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.input.read(buffer)?;
        self.replay.write_all(&buffer[..read])?;
        Ok(read)
    }
}

#[cfg(feature = "dbt")]
fn dbt_evidence_log_level(
    requested: Option<LevelFilter>,
    diagnostic_full_trace: bool,
) -> DbtEvidenceLogLevel {
    let level = verification_log_level(
        requested,
        LogCompareStrictness::Canonical,
        diagnostic_full_trace,
    );
    if level >= LevelFilter::TRACE {
        DbtEvidenceLogLevel::Trace
    } else if level >= LevelFilter::DEBUG {
        DbtEvidenceLogLevel::Debug
    } else {
        DbtEvidenceLogLevel::Info
    }
}

#[cfg(feature = "dbt")]
fn decode_dbt_evidence(file: &mut std::fs::File) -> Result<Vec<Vec<u8>>, Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut encoded = Vec::new();
    file.read_to_end(&mut encoded)?;
    if encoded.is_empty() {
        return Err(Error::msg("DBT canonical evidence was empty"));
    }
    reverie_dbt::decode_evidence(&encoded)
        .map(reverie_dbt::DbtEvidence::into_records)
        .map_err(|error| {
            Error::msg(format!(
                "DBT canonical evidence was malformed or truncated: {error}"
            ))
        })
}

#[cfg(feature = "dbt")]
fn materialize_dbt_comparison_log(
    records: &[Vec<u8>],
    mut log: std::fs::File,
    path: &Path,
) -> Result<usize, Error> {
    if records.is_empty() {
        return Err(Error::msg("DBT canonical evidence contained no records"));
    }
    log.set_len(0)?;
    log.seek(SeekFrom::Start(0))?;
    for record in records {
        let Some(payload) = record.strip_suffix(b"\n") else {
            return Err(Error::msg(
                "DBT canonical evidence record was missing its terminal newline",
            ));
        };
        if payload.contains(&b'\n') || payload.contains(&b'\r') {
            return Err(Error::msg(
                "DBT canonical evidence record contained an embedded line boundary",
            ));
        }
        log.write_all(record)?;
    }
    log.flush()?;

    // The verdict this comparison publishes names `all_records_v1`, so every
    // decoded record must reach the log. Filtering here instead of at the
    // envelope would report a policy that was not applied, which is the exact
    // failure the envelope exists to prevent -- and it would be invisible,
    // because the envelope name lives in a different file. Each record was
    // checked above to hold no embedded line boundary, so one record is one
    // line and this count is exact.
    let materialized = std::fs::read(path)
        .map_err(|error| Error::msg(format!("DBT canonical evidence log unreadable: {error}")))?
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    if materialized != records.len() {
        return Err(Error::msg(format!(
            "DBT canonical evidence log holds {materialized} records but {} were decoded; the \
             comparison publishes the all_records_v1 envelope and must not drop any",
            records.len()
        )));
    }

    let compared =
        detcore::logdiff::write_canonical_info(path, &mut std::io::sink()).map_err(|error| {
            Error::msg(format!(
                "DBT canonical evidence did not contain a valid log stream: {error}"
            ))
        })?;
    if compared == 0 {
        return Err(Error::msg(
            "DBT canonical evidence contained no INFO records",
        ));
    }
    Ok(compared)
}

#[cfg(feature = "dbt")]
fn dbt_verification_output(output: Output) -> reverie::process::Output {
    reverie::process::Output {
        status: process_status(output.status),
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

// Preserve the established human-facing DBT verification contract when no
// durable report was requested. Machine-readable verification below uses the
// canonical INFO comparator because its result is consumed as evidence; this
// path retains the existing stdout, exit-status, and native-summary checks.
#[cfg(feature = "dbt")]
fn run_dbt_legacy_verify(
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    verify_allow: VerifyAllow,
    summary: bool,
    stdin_is_terminal: bool,
    verification_stdin: Option<std::fs::File>,
) -> Result<ExitStatus, Error> {
    let mut replay = if stdin_is_terminal && verification_stdin.is_none() {
        None
    } else {
        Some(tempfile::tempfile()?)
    };

    eprintln!(":: DBT Run1...");
    let first = match (replay.as_mut(), verification_stdin) {
        (Some(replay), Some(input)) => run_once(
            runner,
            guest,
            drrun,
            TeeReader {
                input,
                replay: replay.try_clone()?,
            },
        )?,
        (Some(replay), None) => run_once(
            runner,
            guest,
            drrun,
            TeeReader {
                input: std::io::stdin(),
                replay: replay.try_clone()?,
            },
        )?,
        (None, _) => run_once_with_terminal_input(runner, guest, drrun)?,
    };
    if !verify_allow.satisfies(process_status(first.status)) {
        write_output(&first)?;
        return Ok(output_status(&first));
    }
    let first_summary = detcore_summary(&first)?;
    if stdin_is_terminal && first_summary.stdin_reads != 0 {
        write_output(&first)?;
        return Err(Error::msg(format!(
            "DBT verification cannot replay terminal stdin: guest attempted {} fd-0 read syscall(s)",
            first_summary.stdin_reads
        )));
    }

    eprintln!(":: DBT Run2...");
    let second = match replay.as_mut() {
        Some(replay) => {
            replay.seek(SeekFrom::Start(0))?;
            run_once(runner, guest, drrun, replay.try_clone()?)?
        }
        None => run_once_with_terminal_input(runner, guest, drrun)?,
    };
    if !verify_allow.satisfies(process_status(second.status)) {
        write_output(&second)?;
        return Ok(output_status(&second));
    }
    let second_summary = detcore_summary(&second)?;

    if first.status != second.status {
        write_output(&first)?;
        return Err(Error::msg(format!(
            "DBT verification failed: guest exit status differed between runs ({:?} != {:?})",
            first.status, second.status
        )));
    }
    if first.stdout != second.stdout {
        return Err(Error::msg(dbt_stdout_mismatch(
            &first.stdout,
            &second.stdout,
        )));
    }
    if !first_summary.same_observable_behavior(&second_summary) {
        return Err(Error::msg(format!(
            "DBT verification failed: native Detcore summaries differed ({first_summary:?} != {second_summary:?})"
        )));
    }
    if let Err(error) =
        require_matching_dbt_branch_clocks(first_summary.branches, second_summary.branches)
    {
        write_output(&first)?;
        return Err(error);
    }

    write_output(&first)?;
    eprintln!(
        ":: Comparing DBT observed guest-memory hashes... {} | {}",
        first_summary.memory_hash, second_summary.memory_hash
    );
    eprintln!(":: DBT path confirmed: DynamoRIO client reported tool=Detcore");
    eprintln!(":: Success: deterministic. Determinism verified.");
    if summary {
        eprint!("{}", format_dbt_stats(&first_summary));
    }
    Ok(output_status(&first))
}

/// Runs `program` through DynamoRIO with the real Detcore Tool.
///
/// When `--verify-json` requests a durable report, verification obtains
/// structured tracing records through Reverie's authenticated per-run evidence
/// channel, decodes the finalized framed artifact after the complete process
/// tree is reaped, and hands the resulting log plus exact stdout/stderr/status
/// to Hermit's ordinary typed comparator. Without a report path, the existing
/// human-facing DBT verification contract remains unchanged.
// This mirrors the option surface of `hermit run`, so its parameters track the
// CLI run flags rather than a cohesive value object; bundling them would not
// clarify the dispatch shim.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "dbt")]
pub(super) fn run_dbt(
    program: &Path,
    args: &[String],
    verify: bool,
    verify_verbose: bool,
    verify_allow: VerifyAllow,
    print_verify_logs: bool,
    keep_logs: bool,
    verify_log_dir: Option<&Path>,
    verify_json: Option<&Path>,
    summary: bool,
    log: Option<LevelFilter>,
    log_file: Option<&Path>,
    config: &Config,
    mut environment: BTreeMap<OsString, OsString>,
    verification_stdin: Option<std::fs::File>,
) -> Result<ExitStatus, Error> {
    if let Some(path) = verify_json.filter(|_| verify) {
        write_pending_verification_json(path)?;
    }
    if log_file.is_some() && !verify {
        return Err(Error::msg(
            "DBT --log-file is unavailable on the ordinary single-run adapter",
        ));
    }
    // The DBT backend drives a single Detcore external scheduler, so it cannot
    // honor a request to relax thread sequentialization. Fail loudly rather
    // than silently ignoring the flag.
    if !config.sequentialize_threads {
        return Err(Error::msg(
            "the dbt backend requires sequentialized threads; \
             remove --no-sequentialize-threads (or --strace-only) to run under --backend dbt",
        ));
    }
    let config_json = serde_json::to_string(config).map_err(|error| {
        Error::msg(format!(
            "failed to serialize the Detcore config for the DBT backend: {error}"
        ))
    })?;
    // The full DetConfig now reaches the DBT runtime via the serialized env
    // above; the fail-closed policy (PR #644) still drives process-group
    // isolation and the client flag here.
    let panic_on_unsupported_syscalls = config.panic_on_unsupported_syscalls;

    let stdin_is_terminal = std::io::stdin().is_terminal();

    let (drrun, client) = detcore_dbt::prepare_native_client().map_err(|error| {
        Error::msg(format!(
            "failed to prepare the Detcore DynamoRIO client: {error}"
        ))
    })?;
    let mut runner = DbtRunner::new(&drrun, &client)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure the DynamoRIO DBT runner (drrun={}, client={}): {error}",
                drrun.display(),
                client.display()
            ))
        })?
        .summary(summary && !verify)
        .isolated_process_group(panic_on_unsupported_syscalls);
    if panic_on_unsupported_syscalls {
        runner = runner.client_argument("-panic-on-unsupported-syscalls");
    }

    eprintln!(
        "hermit: [dbt backend] Detcore Tool active; running {program:?} under DynamoRIO ({})",
        drrun.display()
    );

    let _unsupported_report = DbtUnsupportedSyscallReport::new()?;
    let prepared = prepare_dbt_guest_command(
        program,
        args,
        environment.get(OsStr::new("PATH")).map(OsString::as_os_str),
    );
    let mut guest = StdCommand::new(&prepared.program);
    if (!verify || verify_json.is_none())
        && let Some(level) = log
    {
        environment.insert("HERMIT_LOG".into(), level.to_string().into());
    }
    environment.remove(OsStr::new("HERMIT_LOG_FILE"));
    environment.insert(detcore_dbt::DETCONFIG_ENV.into(), config_json.into());
    apply_exact_environment(&mut guest, &environment);
    guest.args(&prepared.args);

    if !verify {
        if stdin_is_terminal {
            let status = runner
                .status(&guest)
                .map_err(|error| dbt_run_error(&drrun, error))?;
            if summary {
                eprintln!(
                    ":: DBT summary: see the `reverie-dbt: tool=Detcore ...` line above \
                     (run without a terminal on stdin for the labeled block)"
                );
            }
            return Ok(process_status(status));
        }
        let output = run_once(&runner, &guest, &drrun, std::io::stdin())?;
        write_output(&output)?;
        if summary {
            match detcore_summary(&output) {
                Ok(stats) => eprint!("{}", format_dbt_stats(&stats)),
                Err(error) => eprintln!(":: DBT summary unavailable: {error}"),
            }
        }
        return Ok(output_status(&output));
    }

    if verify_json.is_none() {
        return run_dbt_legacy_verify(
            &runner.clone().summary(true),
            &guest,
            &drrun,
            verify_allow,
            summary,
            stdin_is_terminal,
            verification_stdin,
        );
    }

    let (log1, log2) = temp_log_files_in("dbt-run1", "dbt-run2", verify_log_dir)
        .map_err(|error| Error::msg(format!("failed to create DBT verification logs: {error}")))?;
    let (log1_file, log1_path) = log1.into_parts();
    let (log2_file, log2_path) = log2.into_parts();
    let evidence_level = dbt_evidence_log_level(log, verify_verbose);
    let mut evidence1 = tempfile::tempfile()?;
    let stats1 = DbtStatsCapture::new()?;
    let runner1 = stats1
        .configure(runner.clone())
        .evidence_file(&evidence1)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure protected DBT run-1 evidence: {error}"
            ))
        })?
        .evidence_log_level(evidence_level);
    let mut evidence2 = tempfile::tempfile()?;
    let stats2 = DbtStatsCapture::new()?;
    let runner2 = stats2
        .configure(runner)
        .evidence_file(&evidence2)
        .map_err(|error| {
            Error::msg(format!(
                "failed to configure protected DBT run-2 evidence: {error}"
            ))
        })?
        .evidence_log_level(evidence_level);

    let mut replay = tempfile::tempfile()?;
    let replayable_stdin = verification_stdin.filter(|file| {
        // SAFETY: `as_raw_fd` borrows a live descriptor for this check.
        (unsafe { libc::isatty(file.as_raw_fd()) }) != 1
    });

    eprintln!(":: DBT Run1...");
    let first_raw = match replayable_stdin {
        Some(input) => run_once(
            &runner1,
            &guest,
            &drrun,
            TeeReader {
                input,
                replay: replay.try_clone()?,
            },
        ),
        None => run_once(&runner1, &guest, &drrun, std::io::empty()),
    };
    let first_raw = match first_raw {
        Ok(output) => output,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
    };
    let first_records = match decode_dbt_evidence(&mut evidence1) {
        Ok(records) => records,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
    };
    if let Err(error) = materialize_dbt_comparison_log(&first_records, log1_file, &log1_path) {
        if keep_logs {
            retain_verification_logs([("run 1", log1_path)])?;
        }
        return Err(error);
    }
    if print_verify_logs {
        std::io::stderr().write_all(&fs::read(&log1_path)?)?;
    }
    if !verify_allow.satisfies(process_status(first_raw.status)) {
        let first = dbt_verification_output(first_raw);
        eprintln!(
            "First run errored during --verify, not continuing to a second. Stdout:\n{}\nStderr:\n{}",
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr),
        );
        if keep_logs {
            retain_verification_logs([("run 1", log1_path)])?;
        }
        return Err(Error::msg("First run during --verify exited in error"));
    }
    let first_stats = match stats1.finish() {
        Ok(stats) => stats,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path)])?;
            }
            return Err(error);
        }
    };
    let first = dbt_verification_output(first_raw);

    replay.seek(SeekFrom::Start(0))?;
    eprintln!(":: DBT Run2...");
    let second_raw = match run_once(&runner2, &guest, &drrun, replay.try_clone()?) {
        Ok(output) => output,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
    };
    let second_records = match decode_dbt_evidence(&mut evidence2) {
        Ok(records) => records,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
    };
    if let Err(error) = materialize_dbt_comparison_log(&second_records, log2_file, &log2_path) {
        if keep_logs {
            retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
        }
        return Err(error);
    }
    let second_stats = match stats2.finish() {
        Ok(stats) => stats,
        Err(error) => {
            if keep_logs {
                retain_verification_logs([("run 1", log1_path), ("run 2", log2_path)])?;
            }
            return Err(error);
        }
    };
    let second = dbt_verification_output(second_raw);

    let branch_clock_comparison = DbtCountedBranchComparison {
        left: first_stats.counted_branches(),
        right: second_stats.counted_branches(),
    };
    let branch_clock_diverged = !branch_clock_comparison.matched();
    let outcome = compare_two_runs(
        ComparedRun {
            output: &first,
            log: log1_path,
            label: "run 1",
        },
        ComparedRun {
            output: &second,
            log: log2_path,
            label: "run 2",
        },
        ComparisonOptions {
            verbose: verify_verbose,
            strictness: LogCompareStrictness::Canonical,
            compare_logs: true,
            diagnostic_full_trace: verify_verbose,
            compare_io_buffers: config.detlog_io_buffers,
            // Read from the LIVE config, for the same reason the run path does:
            // this is a genuine runtime setting, so a hard-coded value would
            // publish a time policy the run did not use.
            virtualize_time: config.virtualize_time,
            // A backend-observed divergence needs the same retained evidence
            // as a divergence found by the ordinary comparator.
            keep_logs: keep_logs || branch_clock_diverged,
            // Every decoded evidence record is compared, which is what this
            // adapter already did before the envelope was disclosed. Naming it
            // changes no record selection; it states the selection in the
            // verdict rather than leaving it implicit.
            //
            // The transport does put records about itself in this stream:
            // `evidence_emit_image_initialization` (reverie-dbt
            // native/client.c:863) emits
            // `INFO reverie_dbt::evidence: protected evidence initialized`
            // once per sender -- the sender is keyed on (pid, start_time) and
            // latched by `initialization_record_sent`, so it is once per
            // process, not per image -- and its `evidence_log_level < 3` guard
            // is open at the verification default of INFO. The record is a
            // compile-time constant string, so two runs of a single-process
            // guest compare equal. Excluding them is
            // therefore a separable change, not a prerequisite: it would alter
            // which records this adapter compares, and it needs its own
            // evidence about multi-process arrival order, which is host order.
            record_envelope: RecordEnvelope::all_records_v1(),
        },
    )?;
    let (outcome, branch_clock_failure) =
        finalize_dbt_verification(outcome, branch_clock_comparison, verify_json)?;
    // Publish the typed record before announcing any terminal verdict. If the
    // process is killed after this line, a reader still sees this invocation's
    // divergence rather than the pending `no_result` stamp.
    if let Some(message) = branch_clock_failure {
        eprintln!(":: {message}");
    }
    let success_message = if config.detlog_io_buffers {
        "Success: deterministic. Determinism verified."
    } else {
        "Success: deterministic. Determinism verified. NOTE: syscall \
         output-buffer CONTENT was not compared because --no-detlog-io-buffers \
         was given, so a divergence confined to a buffer whose length is stable \
         would not have been seen; drop that flag to include it."
    };
    announce_verification_outcome(&outcome, success_message, "Failure: nondeterministic.");
    if !outcome.verified() {
        return outcome.into_exit_status();
    }

    std::io::stdout().write_all(&first.stdout)?;
    std::io::stderr().write_all(&first.stderr)?;
    Ok(outcome.guest_status)
}

#[allow(clippy::too_many_arguments)]
#[cfg(not(feature = "dbt"))]
pub(super) fn run_dbt(
    _program: &Path,
    _args: &[String],
    _verify: bool,
    _verify_verbose: bool,
    _verify_allow: VerifyAllow,
    _print_verify_logs: bool,
    _keep_logs: bool,
    _verify_log_dir: Option<&Path>,
    _verify_json: Option<&Path>,
    _summary: bool,
    _log: Option<LevelFilter>,
    _log_file: Option<&Path>,
    _config: &Config,
    _environment: BTreeMap<OsString, OsString>,
    _verification_stdin: Option<std::fs::File>,
) -> Result<ExitStatus, Error> {
    Err(Error::msg("DBT support was not included in this build"))
}

#[cfg(feature = "dbt")]
fn dbt_stdout_mismatch(first: &[u8], second: &[u8]) -> String {
    const CONTEXT_BEFORE: usize = 40;
    const CONTEXT_AFTER: usize = 120;

    let offset = first
        .iter()
        .zip(second)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| first.len().min(second.len()));
    let start = offset.saturating_sub(CONTEXT_BEFORE);
    let first_end = first.len().min(offset.saturating_add(CONTEXT_AFTER));
    let second_end = second.len().min(offset.saturating_add(CONTEXT_AFTER));

    format!(
        concat!(
            "DBT verification failed: guest stdout differed at byte {offset} ",
            "(run1_len={}, run2_len={}); run1[{start}..{first_end}]={:?}; ",
            "run2[{start}..{second_end}]={:?}"
        ),
        first.len(),
        second.len(),
        String::from_utf8_lossy(&first[start..first_end]),
        String::from_utf8_lossy(&second[start..second_end]),
        offset = offset,
        start = start,
        first_end = first_end,
        second_end = second_end,
    )
}

#[cfg(feature = "dbt")]
fn run_once<R: Read + Send + 'static>(
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
    input: R,
) -> Result<Output, Error> {
    runner
        .output_with_detached_reader(guest, input)
        .map_err(|error| dbt_run_error(drrun, error))
}

#[cfg(feature = "dbt")]
fn run_once_with_terminal_input(
    runner: &DbtRunner,
    guest: &StdCommand,
    drrun: &Path,
) -> Result<Output, Error> {
    runner
        .output_with_inherited_stdin(guest)
        .map_err(|error| dbt_run_error(drrun, error))
}

/// Name the stage that actually failed.
///
/// This used to be `launch_error`, and it announced EVERY `io::Error` from a
/// DBT run as "failed to launch drrun ({path})". But the calls it wraps --
/// `output_with_detached_reader` and `output_with_inherited_stdin` -- run the
/// whole lifecycle: spawn, wait, collect output, and finalize the protected
/// evidence session. A failure in any later stage was reported as a failure of
/// the first one.
///
/// Measured cost of that: a campaign agent hit
/// `"failed to launch drrun (target/install_pkg/rsrcs/dynamorio/bin64/drrun):
/// DBT guest exited with status S..."`, correctly observed that drrun was a
/// real 737 KB ELF matching the build cache, and could not proceed. The inner
/// text is the giveaway -- it comes from `reverie-dbt`'s
/// `(Ok(status), Err(error))` arm, so the guest HAD RUN AND EXITED and the
/// failure was in evidence finalization. A binary that failed to launch cannot
/// produce an exit status. The wrapper had renamed a post-run failure into a
/// missing-binary hunt.
///
/// The discriminator is `ErrorKind`. A real spawn failure surfaces the OS error
/// (`NotFound` when drrun is absent, `PermissionDenied` when it is not
/// executable, `ExecutableFileBusy`). Where the kind does not prove a spawn
/// failure this deliberately does NOT claim one.
///
/// THE RESIDUAL, STATED BECAUSE IT IS NOT FIXED HERE. Most post-launch failures
/// arrive as `Other` -- including the evidence-finalization error that motivated
/// this function, which is built with `io::Error::other` -- but NOT all of them.
/// `reverie-dbt` launcher.rs, in the `(Ok(status), Err(error))` arm, re-emits the
/// inner evidence error's OWN kind rather than forcing `Other`:
///
///     (Ok(status), Err(error)) => Err(io::Error::new(error.kind(), ...))
///
/// and the evidence layer can produce spawn-shaped kinds after the guest is
/// already running: `PermissionDenied` from its peer-credential checks, and
/// `NotFound` from a bare `?` on `/proc/<pid>/stat` when the peer exits before
/// it is read. Both still misreport here as launch failures.
///
/// That `NotFound` case is the sharpest one, and worth naming so nobody has to
/// rediscover it: the pid exists ONLY because the guest launched, so the error
/// that most conclusively proves a successful launch is the one that would be
/// blamed on the binary.
///
/// This is narrower than what it replaces -- previously EVERY post-launch error
/// claimed a launch failure -- but it is not zero. The fix belongs in
/// reverie-dbt, whose arm already holds `Ok(status)` and therefore knows the
/// guest ran: it should not re-emit a spawn-shaped kind at all. Do NOT patch it
/// here by matching on the message text; classifying a typed error by its
/// display string is the shape this project removes elsewhere.
#[cfg(feature = "dbt")]
fn dbt_run_error(drrun: &Path, error: std::io::Error) -> Error {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::ExecutableFileBusy => {
            Error::msg(format!(
                "failed to launch drrun ({}): {error}",
                drrun.display()
            ))
        }
        _ => Error::msg(format!(
            "drrun started ({}) and the DBT run then failed: {error} \
             -- this is NOT a launch failure; the drrun binary is not implicated. \
             Read the text after the last ':' for the stage that actually failed.",
            drrun.display()
        )),
    }
}

// ⚠️ `test` AS WELL AS `dbt`, AND THE REASON IS THAT THE BRACKETS BELOW MUST RUN.
// `dbt` is not in `default` (hermit-cli/Cargo.toml: `default = []`), so under
// `#[cfg(feature = "dbt")]` alone this function and any test of it compile only in
// a build validation does not perform. Measured on main before this change:
//
//     $ cargo test -p hermit --bin hermit dbt_status
//     running 0 tests
//
// Zero -- so the only check that a signalled death is not reported as a normal exit
// contributed nothing to any receipt. Adding `test` keeps the symbol out of a
// feature-off PRODUCTION build, where `clippy -D warnings` would call the import
// dead, while letting the brackets execute wherever tests do.
#[cfg(any(feature = "dbt", test))]
fn process_status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus::from_raw(status.into_raw())
}

#[cfg(feature = "dbt")]
fn detcore_summary(output: &Output) -> Result<DbtSummary, Error> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr
        .lines()
        .rev()
        .find(|line| line.starts_with(DBT_SUMMARY_PREFIX))
        .ok_or_else(|| {
            Error::msg(
                "DBT verification failed: native DynamoRIO summary did not report tool=Detcore",
            )
        })?;

    let field = |name: &str| {
        summary
            .split_ascii_whitespace()
            .find_map(|value| value.strip_prefix(name))
            .ok_or_else(|| Error::msg(format!("DBT verification failed: summary omitted {name}")))
    };
    let branches = field("branches=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid branch count"))?;
    let syscalls = field("syscalls=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid syscall count"))?;
    let rewritten = field("rewritten=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid rewritten count"))?;
    let stdin_reads = field("stdin_reads=")?
        .parse::<u64>()
        .map_err(|_| Error::msg("DBT verification failed: invalid stdin read count"))?;
    // A guest can reach the native callback without asking Detcore to suppress
    // a syscall. For example, a raw program whose only syscall is the native
    // lifecycle `exit` reports `rewritten=0`. The callback is still healthy as
    // long as it observed work and did not report more rewrites than syscalls.
    if branches == 0 || syscalls == 0 || rewritten > syscalls {
        return Err(Error::msg(
            "DBT verification failed: native callback counters are inconsistent",
        ));
    }

    let hash = field("memory_hash=")?;
    if hash.len() != 16 || u64::from_str_radix(hash, 16).is_err() {
        return Err(Error::msg(
            "DBT verification failed: invalid observed-memory hash",
        ));
    }
    Ok(DbtSummary {
        branches,
        syscalls,
        rewritten,
        stdin_reads,
        memory_hash: hash.to_owned(),
    })
}

#[cfg(feature = "dbt")]
fn write_output(output: &Output) -> Result<(), Error> {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    Ok(())
}

#[cfg(feature = "dbt")]
fn output_status(output: &Output) -> ExitStatus {
    // ⚠️ SAME SIGNAL-LOSING CONVERSION `process_status` ALREADY FIXED, MISSED HERE.
    // `std::process::ExitStatus::code()` is `None` for a process killed by a
    // signal, so `code().unwrap_or(1)` reported every signalled death as a
    // normal `Exited(1)`: a guest killed by SIGSEGV came back as "exited
    // normally with status 1", and WIFSIGNALED/WTERMSIG/WCOREDUMP were all lost.
    // `ExitStatus::from_raw` decodes exited-versus-signalled and the core-dump
    // flag the same way the ptrace backend does.
    ExitStatus::from_raw(output.status.into_raw())
}

fn sabre_artifact(variable: &str, description: &str, executable: bool) -> Result<OsString, Error> {
    let value = std::env::var_os(variable).ok_or_else(|| {
        Error::msg(format!(
            "the sabre backend needs {variable}=<path-to-{description}>"
        ))
    })?;
    validate_sabre_artifact(Path::new(&value), variable, executable)
}

fn validate_sabre_artifact(
    requested_path: &Path,
    variable: &str,
    executable: bool,
) -> Result<OsString, Error> {
    let path = fs::canonicalize(requested_path).map_err(|error| {
        Error::msg(format!(
            "the sabre backend cannot access {variable}={}: {error}",
            requested_path.display()
        ))
    })?;
    let metadata = fs::metadata(&path).map_err(|error| {
        Error::msg(format!(
            "the sabre backend cannot inspect {variable}={}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(Error::msg(format!(
            "the sabre backend needs {variable}={} to be a regular file",
            path.display()
        )));
    }
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::msg(format!(
            "the sabre backend needs {variable}={} to be executable",
            path.display()
        )));
    }
    Ok(path.into_os_string())
}

const SABRE_QUIET_ENV: &str = "REVERIE_SABRE_STRACE_QUIET";

fn sabre_command(
    runner: &OsString,
    sabre: &OsString,
    plugin: &OsString,
    program: &Path,
    args: &[String],
    quiet: bool,
    log: Option<LevelFilter>,
) -> StdCommand {
    let mut command = StdCommand::new(runner);
    command
        .arg("--sabre")
        .arg(sabre)
        .arg("--plugin")
        .arg(plugin)
        .arg("--")
        .arg(program)
        .args(args);
    if quiet {
        command.env(SABRE_QUIET_ENV, "1");
    }
    if let Some(level) = log {
        command.env("HERMIT_LOG", level.to_string());
    }
    command
}

fn sabre_artifacts() -> Result<(OsString, OsString, OsString), Error> {
    Ok((
        sabre_artifact("HERMIT_SABRE_RUNNER", "reverie-sabre-strace", true)?,
        sabre_artifact("HERMIT_SABRE_BINARY", "sabre", true)?,
        sabre_artifact(
            "HERMIT_SABRE_PLUGIN",
            "libreverie_sabre_strace_plugin.so",
            false,
        )?,
    ))
}

/// Runs program through the shared Reverie strace tool over SaBRe.
///
/// The SaBRe host and plugin live in the coordinated Reverie checkout, so
/// Hermit uses explicit artifact paths rather than taking an unreleased Cargo
/// dependency:
///
/// * HERMIT_SABRE_RUNNER: reverie-sabre-strace executable.
/// * HERMIT_SABRE_BINARY: pinned SaBRe executable.
/// * HERMIT_SABRE_PLUGIN: libreverie_sabre_strace_plugin.so.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#589): Review SaBRe CLI backend dispatch.
pub fn run_sabre_strace(program: &Path, args: &[String]) -> Result<ExitStatus, Error> {
    let (runner, sabre, plugin) = sabre_artifacts()?;

    eprintln!("hermit: [sabre backend] tracing {program:?} with the shared Reverie tool");

    let status = sabre_command(&runner, &sabre, &plugin, program, args, false, None)
        .status()
        .map_err(|error| {
            Error::msg(format!(
                "failed to launch the SaBRe runner {}: {error}",
                Path::new(&runner).display()
            ))
        })?;

    Ok(status.into())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "dbt")]
    /// A behavioural pin, not a text match: drive the real materialization with
    /// a transport self-record present and require every decoded record to
    /// reach the log. Filtering anywhere in this function -- at the write loop,
    /// at the decoder, or through a filtered canonical writer -- makes the
    /// materialized count disagree with the decoded count and fails here, while
    /// the verdict still names `all_records_v1`.
    #[test]
    fn materialization_keeps_every_decoded_record_including_transport_self_records() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("evidence.log");
        let file = std::fs::File::create(&path).expect("create log");
        let records: Vec<Vec<u8>> = vec![
            b"1970-01-01T00:00:00.000000Z INFO reverie_dbt::evidence: protected evidence initialized\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG first\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z  INFO detcore: DETLOG second\n".to_vec(),
        ];

        super::materialize_dbt_comparison_log(&records, file, &path)
            .expect("materialization must accept a stream containing transport self-records");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            written.lines().count(),
            records.len(),
            "every decoded record must reach the compared log"
        );
        assert!(
            written.contains("reverie_dbt::evidence: protected evidence initialized"),
            "the transport self-record is part of what all_records_v1 compares; dropping it \
             here would publish an envelope that was not applied:\n{written}"
        );
    }
    use super::*;

    #[cfg(feature = "dbt")]
    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(feature = "dbt")]
    fn dbt_summary(branches: u64) -> DbtSummary {
        DbtSummary {
            branches,
            syscalls: 169,
            rewritten: 168,
            stdin_reads: 0,
            memory_hash: "4b5e0e70f3050157".to_owned(),
        }
    }

    #[cfg(feature = "dbt")]
    fn typed_outcome(verdict: Verdict, guest_exit: i32) -> VerificationOutcome {
        use super::super::verify::ComparedLogCounts;
        use super::super::verify::ComparisonSpec;

        VerificationOutcome {
            verdict,
            guest_status: ExitStatus::Exited(guest_exit),
            comparison: ComparisonSpec::new(
                LogCompareStrictness::Canonical,
                true,
                false,
                true,
                RecordEnvelope::all_records_v1().policy(),
                true,
            ),
            compared_log_messages: Some(ComparedLogCounts { left: 4, right: 4 }),
            dbt_counted_branches: None,
            compared_labels: detcore::logdiff::ComparisonSideLabels::default(),
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_keeps_branch_clock_out_of_the_other_summary_fields() {
        let first = dbt_summary(563_145);
        let different_clock = dbt_summary(563_103);

        assert!(first.same_observable_behavior(&different_clock));
        let message = dbt_branch_clock_mismatch(first.branches, different_clock.branches).unwrap();
        assert!(
            message.contains("counted-branch clocks differed"),
            "{message}"
        );
        assert!(message.contains("563145 != 563103"), "{message}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn legacy_branch_clock_mismatch_is_a_hard_failure() {
        let error = require_matching_dbt_branch_clocks(563_145, 563_103).unwrap_err();
        let rendered = error.to_string();

        assert!(
            rendered.contains("counted-branch clocks differed"),
            "{rendered}"
        );
        assert!(!rendered.contains("Success"), "{rendered}");
        assert!(require_matching_dbt_branch_clocks(563_145, 563_145).is_ok());
    }

    /// Pin the production control flow without launching DynamoRIO. This uses
    /// the same source-order contract style as the CLI dispatch and report-first
    /// tests: every slice ends before the test module, so its own needles cannot
    /// satisfy the assertions.
    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_verification_run_path_binds_terminal_verdict_to_branch_stats() {
        let source = include_str!("backends.rs");
        let legacy = source
            .split_once("fn run_dbt_legacy_verify(")
            .expect("legacy DBT verifier")
            .1
            .split_once("/// Runs `program` through DynamoRIO")
            .expect("end of legacy DBT verifier")
            .0;
        let legacy_guard = legacy
            .find("require_matching_dbt_branch_clocks(")
            .expect("legacy counted-branch guard");
        let legacy_success = legacy
            .find(":: Success: deterministic. Determinism verified.")
            .expect("legacy success announcement");
        assert!(
            legacy_guard < legacy_success,
            "legacy verification must reject a branch-clock mismatch before announcing success"
        );

        let finalizer = source
            .split_once("fn finalize_dbt_verification(")
            .expect("typed DBT finalizer")
            .1
            .split_once("/// Own the typed statistics stream")
            .expect("end of typed DBT finalizer")
            .0;
        let attach = finalizer
            .find("record_dbt_branch_clock_comparison(")
            .expect("attach branch-clock comparison");
        let publish = finalizer
            .find("write_verification_json(path, &outcome)")
            .expect("publish terminal typed verdict");
        let return_outcome = finalizer
            .find("Ok((outcome, failure))")
            .expect("return finalized outcome");
        assert!(
            attach < publish && publish < return_outcome,
            "the finalizer must attach the branch comparison and publish JSON before returning it"
        );

        let canonical_marker = concat!("#[cfg(feature = \"dbt\")]\n", "pub(super) fn ", "run_dbt(");
        let canonical = source
            .split_once(canonical_marker)
            .expect("canonical DBT run path")
            .1
            .split_once(r#"#[cfg(not(feature = "dbt"))]"#)
            .expect("end of canonical DBT run path")
            .0;
        let first_stats = canonical
            .find("let first_stats = match stats1.finish()")
            .expect("run-1 typed stats");
        let second_stats = canonical
            .find("let second_stats = match stats2.finish()")
            .expect("run-2 typed stats");
        let comparison = canonical
            .find("let branch_clock_comparison = DbtCountedBranchComparison")
            .expect("typed branch-clock comparison");
        let force_logs = canonical
            .find("keep_logs: keep_logs || branch_clock_diverged")
            .expect("branch divergence forces log retention");
        let finalize = canonical
            .find("finalize_dbt_verification(outcome, branch_clock_comparison, verify_json)")
            .expect("terminal typed finalization");
        let announce = canonical
            .find("if let Some(message) = branch_clock_failure")
            .expect("branch-specific terminal announcement");
        let exit = canonical
            .find("return outcome.into_exit_status()")
            .expect("terminal nonzero conversion");
        assert!(
            first_stats < second_stats
                && second_stats < comparison
                && comparison < force_logs
                && force_logs < finalize
                && finalize < announce
                && announce < exit,
            "canonical verification must collect both stats, compute the comparison, force logs, \
             publish through the finalizer, and only then announce or convert the terminal status"
        );

        for (stats, next) in [
            (
                "let first_stats = match stats1.finish()",
                "let first = dbt_verification_output",
            ),
            (
                "let second_stats = match stats2.finish()",
                "let second = dbt_verification_output",
            ),
        ] {
            let failure_arm = canonical
                .split_once(stats)
                .expect("typed stats collection")
                .1
                .split_once(next)
                .expect("end of typed stats failure arm")
                .0;
            assert!(failure_arm.contains("Err(error) =>"));
            assert!(
                failure_arm.contains("return Err(error);"),
                "unreadable typed stats must return while the pre-stamped no_result is still current"
            );
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn equal_branch_clocks_do_not_authorize_a_no_result() {
        let outcome = typed_outcome(Verdict::NoResult, 23);
        let comparison = DbtCountedBranchComparison {
            left: 563_145,
            right: 563_145,
        };
        let verdict_file = tempfile::NamedTempFile::new().unwrap();

        let (outcome, failure) =
            finalize_dbt_verification(outcome, comparison, Some(verdict_file.path())).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(verdict_file.path()).unwrap()).unwrap();

        assert!(failure.is_none());
        assert_eq!(outcome.verdict, Verdict::NoResult);
        assert!(outcome.dbt_counted_branches.is_none());
        assert_eq!(json["verdict"], "no_result");
        assert_eq!(json["verified"], false);
        assert!(json.get("dbt_counted_branches").is_none());
        let error = outcome.into_exit_status().unwrap_err().to_string();
        assert!(error.contains("did not reach a verdict"), "{error}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn branch_clock_divergence_is_retained_in_the_serialized_typed_verdict() {
        use super::super::verify::ComparedLogCounts;

        for initial_verdict in [Verdict::Matched, Verdict::NoResult] {
            let outcome = typed_outcome(initial_verdict, 0);
            let comparison = DbtCountedBranchComparison {
                left: 563_145,
                right: 563_103,
            };
            let verdict_file = tempfile::NamedTempFile::new().unwrap();
            let (outcome, message) =
                finalize_dbt_verification(outcome, comparison, Some(verdict_file.path())).unwrap();
            let message = message.unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&fs::read(verdict_file.path()).unwrap()).unwrap();

            assert!(message.contains("563145 != 563103"), "{message}");
            assert_eq!(outcome.verdict, Verdict::Diverged);
            assert_eq!(outcome.dbt_counted_branches, Some(comparison));
            assert_eq!(json["verdict"], "diverged");
            assert_eq!(json["verified"], false);
            assert_eq!(json["bitwise_parity"], false);
            assert_eq!(json["dbt_counted_branches"]["left"], 563_145);
            assert_eq!(json["dbt_counted_branches"]["right"], 563_103);
            assert_eq!(
                outcome.compared_log_messages,
                Some(ComparedLogCounts { left: 4, right: 4 })
            );
            let error = outcome.into_exit_status().unwrap_err().to_string();
            assert!(error.contains("counted-branch clocks differed"), "{error}");
            assert!(!error.contains("outputs"), "{error}");
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn equal_branch_clocks_preserve_match_and_nonzero_guest_status() {
        let outcome = typed_outcome(Verdict::Matched, 23);
        let comparison = DbtCountedBranchComparison {
            left: 563_145,
            right: 563_145,
        };

        let (outcome, failure) = finalize_dbt_verification(outcome, comparison, None).unwrap();
        assert!(failure.is_none());
        let report = super::super::verify::VerificationReport::from(&outcome);

        assert_eq!(outcome.verdict, Verdict::Matched);
        assert_eq!(report.dbt_counted_branches, Some(comparison));
        assert!(report.verified);
        assert!(report.bitwise_parity);
        assert_eq!(outcome.into_exit_status().unwrap(), ExitStatus::Exited(23));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn typed_stats_capture_aggregates_the_whole_process_tree() {
        use reverie_dbt::backend_stats::DbtProcessRecord;
        use reverie_dbt::backend_stats::encode_process_record;

        let capture = DbtStatsCapture::new().unwrap();
        let root = DbtProcessRecord {
            branches: 400,
            syscalls: 7,
            ..Default::default()
        };
        let child = DbtProcessRecord {
            branches: 23,
            syscalls: 2,
            ..Default::default()
        };
        let bytes = [encode_process_record(&root), encode_process_record(&child)].concat();
        fs::write(&capture.path, bytes).unwrap();

        let snapshot = capture.finish().unwrap();

        assert_eq!(snapshot.process_images(), 2);
        assert_eq!(snapshot.counted_branches(), 423);
        assert_eq!(snapshot.intercepted_syscalls(), 9);
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn typed_stats_capture_refuses_missing_empty_or_truncated_evidence() {
        let missing = DbtStatsCapture::new().unwrap();
        let error = missing.finish().unwrap_err();
        assert!(
            error.to_string().contains("statistics were missing"),
            "{error}"
        );

        let empty = DbtStatsCapture::new().unwrap();
        fs::write(&empty.path, b"").unwrap();
        let error = empty.finish().unwrap_err();
        assert!(
            error.to_string().contains("statistics were empty"),
            "{error}"
        );

        let truncated = DbtStatsCapture::new().unwrap();
        fs::write(&truncated.path, b"truncated").unwrap();
        let error = truncated.finish().unwrap_err();
        assert!(
            error.to_string().contains("statistics were unreadable"),
            "{error}"
        );
        assert!(error.to_string().contains("truncated"), "{error}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn typed_stats_failure_leaves_the_pre_stamped_no_result() {
        let verdict_file = tempfile::NamedTempFile::new().unwrap();
        write_pending_verification_json(verdict_file.path()).unwrap();

        let empty = DbtStatsCapture::new().unwrap();
        fs::write(&empty.path, b"").unwrap();
        assert!(empty.finish().is_err());

        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(verdict_file.path()).unwrap()).unwrap();
        assert_eq!(json["verdict"], "no_result");
        assert_eq!(json["verified"], false);
        assert_eq!(json["bitwise_parity"], false);
        assert!(json.get("dbt_counted_branches").is_none());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_materializes_records_unchanged() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = log.into_parts();
        let records = vec![
            b"1970-01-01T00:00:00.000000Z INFO detcore: DETLOG first\n".to_vec(),
            b"1970-01-01T00:00:00.000000Z INFO detcore::scheduler: second\n".to_vec(),
        ];

        let compared = materialize_dbt_comparison_log(&records, file, &path).unwrap();

        assert_eq!(compared, 2);
        assert_eq!(fs::read(&path).unwrap(), records.concat());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_fails_closed_on_empty_or_unframed_records() {
        let empty = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = empty.into_parts();
        assert!(materialize_dbt_comparison_log(&[], file, &path).is_err());

        let unframed = tempfile::NamedTempFile::new().unwrap();
        let (file, path) = unframed.into_parts();
        assert!(
            materialize_dbt_comparison_log(
                &[b"1970-01-01T00:00:00.000000Z INFO detcore: missing newline".to_vec()],
                file,
                &path,
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_canonical_evidence_fails_closed_on_empty_or_malformed_artifact() {
        let mut empty = tempfile::tempfile().unwrap();
        assert!(decode_dbt_evidence(&mut empty).is_err());

        let mut malformed = tempfile::tempfile().unwrap();
        malformed.write_all(b"not framed evidence").unwrap();
        assert!(decode_dbt_evidence(&mut malformed).is_err());
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stats_block_labels_counters_honestly() {
        let rendered = format_dbt_stats(&dbt_summary(563_145));
        // The branch counter must be labeled as a branch clock, never as
        // "basic blocks translated" — the client counts retired branches.
        assert!(rendered.contains("counted branches (deterministic branch clock): 563145"));
        assert!(rendered.contains("syscalls intercepted:                          169"));
        assert!(rendered.contains("syscall instructions rewritten:                168"));
        assert!(rendered.contains("stdin (fd 0) reads:                            0"));
        assert!(
            rendered.contains("observed guest-memory hash:                    4b5e0e70f3050157")
        );
        assert!(!rendered.to_lowercase().contains("basic block"));
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stats_block_round_trips_from_a_client_summary_line() {
        let output = dbt_output(
            "reverie-dbt: tool=Detcore branches=42 syscalls=7 rewritten=6 \
             stdin_reads=0 memory_hash=cbf29ce484222325\n",
        );
        let parsed = detcore_summary(&output).unwrap();
        let rendered = format_dbt_stats(&parsed);
        assert!(rendered.contains("counted branches (deterministic branch clock): 42"));
        assert!(rendered.contains("syscalls intercepted:                          7"));
        assert!(
            rendered.contains("observed guest-memory hash:                    cbf29ce484222325")
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_compares_observable_counters_and_hash() {
        let expected = dbt_summary(100);

        let mut actual = dbt_summary(100);
        actual.syscalls += 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbt_summary(100);
        actual.rewritten -= 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbt_summary(100);
        actual.stdin_reads += 1;
        assert!(!expected.same_observable_behavior(&actual));

        let mut actual = dbt_summary(100);
        actual.memory_hash = "0000000000000000".to_owned();
        assert!(!expected.same_observable_behavior(&actual));
    }

    #[cfg(feature = "dbt")]
    fn dbt_output(summary: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: summary.as_bytes().to_vec(),
        }
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_accepts_a_callback_without_rewritten_syscalls() {
        let output = dbt_output(
            "reverie-dbt: tool=Detcore branches=18 syscalls=1 rewritten=0 \
             stdin_reads=0 memory_hash=cbf29ce484222325\n",
        );

        let summary = detcore_summary(&output).unwrap();

        assert_eq!(summary.syscalls, 1);
        assert_eq!(summary.rewritten, 0);
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_summary_rejects_more_rewrites_than_syscalls() {
        let output = dbt_output(
            "reverie-dbt: tool=Detcore branches=18 syscalls=1 rewritten=2 \
             stdin_reads=0 memory_hash=cbf29ce484222325\n",
        );

        let error = detcore_summary(&output).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("native callback counters are inconsistent"),
            "{error}"
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stdout_mismatch_reports_offset_lengths_and_bounded_context() {
        let first = [vec![b'a'; 80], b"left-tail".to_vec()].concat();
        let second = [vec![b'a'; 80], b"right-tail-extra".to_vec()].concat();

        let detail = dbt_stdout_mismatch(&first, &second);

        assert!(detail.contains("differed at byte 80"), "{detail}");
        assert!(detail.contains("run1_len=89, run2_len=96"), "{detail}");
        assert!(detail.contains("run1[40..89]"), "{detail}");
        assert!(detail.contains("left-tail"), "{detail}");
        assert!(detail.contains("right-tail-extra"), "{detail}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_stdout_mismatch_reports_prefix_length_difference() {
        let detail = dbt_stdout_mismatch(b"same", b"same suffix");

        assert!(detail.contains("differed at byte 4"), "{detail}");
        assert!(detail.contains("run1_len=4, run2_len=11"), "{detail}");
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_resolves_simple_env_shebang_target_to_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let python = bin.join("python3");
        write_executable(&python, b"\x7fELFplaceholder");
        let script = root.path().join("guest.py");
        write_executable(&script, b"#!/usr/bin/env python3\n");

        let prepared =
            prepare_dbt_guest_command(&script, &["argument".to_owned()], Some(bin.as_os_str()));

        assert_eq!(prepared.program, Path::new("/usr/bin/env"));
        assert_eq!(
            prepared.args,
            [
                python.into_os_string(),
                script.into_os_string(),
                OsString::from("argument"),
            ]
        );
    }

    #[test]
    #[cfg(feature = "dbt")]
    fn dbt_leaves_complex_env_shebang_for_launcher() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("guest.py");
        write_executable(&script, b"#!/usr/bin/env -S python3 -u\n");

        let prepared = prepare_dbt_guest_command(&script, &[], Some(OsStr::new("/usr/bin")));

        assert_eq!(prepared.program, script);
        assert!(prepared.args.is_empty());
    }

    #[test]
    fn sabre_artifact_returns_the_validated_absolute_path() {
        let file = tempfile::NamedTempFile::new_in(".").unwrap();
        let relative_path = file.path().file_name().unwrap();

        let resolved = validate_sabre_artifact(Path::new(relative_path), "test-artifact", false)
            .map(std::path::PathBuf::from)
            .unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, fs::canonicalize(file.path()).unwrap());
    }

    /// A guest killed by a signal must report as SIGNALLED, not as a normal exit.
    ///
    /// ⚠️ THIS IS THE TEST THAT WAS MISSING, WHICH IS WHY ONE SITE GOT FIXED AND
    /// ITS SIBLING DID NOT. `process_status` was converted to `from_raw`;
    /// `output_status` kept `code().unwrap_or(1)` and nothing noticed, because no
    /// test asserted the signalled case for either. Asserting it here binds BOTH.
    #[cfg(feature = "dbt")]
    #[test]
    fn a_signalled_guest_is_not_reported_as_a_normal_exit() {
        use std::os::unix::process::ExitStatusExt as _;

        // raw wait status for "killed by SIGSEGV" (11), no core dump.
        let raw = 11i32;
        let native = std::process::ExitStatus::from_raw(raw);
        assert!(native.code().is_none(), "SIGSEGV death has no exit code");

        let converted = process_status(native);
        assert!(
            !matches!(converted, ExitStatus::Exited(_)),
            "a signalled guest must not read as a normal exit, got {converted:?}"
        );

        // ⚠️ CALL `output_status` ITSELF, not an expression that looks like it.
        // My first version compared `process_status(..)` against an inline
        // `ExitStatus::from_raw(..)` and never invoked `output_status` at all --
        // so reverting `output_status` to the buggy form left this test GREEN.
        // Caught by mutating BOTH sites instead of one.
        let output = Output {
            status: native,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let from_output = output_status(&output);
        assert!(
            !matches!(from_output, ExitStatus::Exited(_)),
            "output_status must not report a signalled guest as a normal exit, got {from_output:?}"
        );
        assert_eq!(
            converted, from_output,
            "output_status and process_status must agree; they diverged once and \
             only one of them was fixed"
        );
    }

    /// A real spawn failure -- drrun absent or not executable -- must still say
    /// so, because that is when the operator SHOULD go and look at the binary.
    #[cfg(feature = "dbt")]
    #[test]
    fn a_spawn_failure_still_names_the_launch() {
        let drrun = Path::new("/nonexistent/dynamorio/bin64/drrun");
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let rendered = dbt_run_error(drrun, std::io::Error::new(kind, "boom")).to_string();
            assert!(
                rendered.contains("failed to launch drrun"),
                "a {kind:?} spawn failure must be reported as a launch failure: {rendered}"
            );
        }
    }

    /// The regression this function exists for. A post-launch failure -- the
    /// evidence-finalization error is the real-world one -- must NOT be
    /// reported as a launch failure. It sent a campaign agent hunting a 737 KB
    /// drrun that was correct and working, and the inner text proves the guest
    /// had already exited.
    #[cfg(feature = "dbt")]
    #[test]
    fn a_post_launch_failure_does_not_blame_the_binary() {
        let drrun = Path::new("/real/dynamorio/bin64/drrun");
        let inner = "DBT guest exited with status Some(1) while protected evidence failed: \
                     DBT evidence collector thread panicked";
        let rendered = dbt_run_error(drrun, std::io::Error::other(inner)).to_string();
        assert!(
            !rendered.contains("failed to launch drrun"),
            "a post-launch failure must not be reported as a launch failure: {rendered}"
        );
        assert!(
            rendered.contains("drrun started"),
            "the message must say the launch succeeded: {rendered}"
        );
        assert!(
            rendered.contains("NOT a launch failure"),
            "the message must rule the binary out explicitly: {rendered}"
        );
        // The cause must survive verbatim. Truncating it here is what cost the
        // original investigation its answer.
        assert!(
            rendered.contains("protected evidence failed"),
            "the underlying error must be preserved in full: {rendered}"
        );
    }

    /// A signalled death must not be reported as a normal exit.
    ///
    /// Ported from hermit#1689's fifth commit as the one free-standing piece of
    /// that head: the behaviour it brackets (`ExitStatus::from_raw` preserving
    /// exited-versus-signalled) is ALREADY on main -- that head's claim 2 landed
    /// via `b441950f72` -- while its bracket never did. Landed code with no
    /// coverage, which is the pairing worth closing first.
    #[test]
    fn dbt_status_preserves_normal_exit_codes() {
        for code in [0, 1, 42, 255] {
            let raw = std::process::ExitStatus::from_raw(code << 8);
            assert_eq!(
                process_status(raw),
                ExitStatus::Exited(code),
                "a normal exit with code {code} must round-trip"
            );
        }
    }

    #[test]
    fn dbt_status_preserves_death_by_signal() {
        for (signum, signal) in [
            (libc::SIGABRT, reverie::Signal::SIGABRT),
            (libc::SIGSEGV, reverie::Signal::SIGSEGV),
            (libc::SIGFPE, reverie::Signal::SIGFPE),
            (libc::SIGILL, reverie::Signal::SIGILL),
            (libc::SIGTERM, reverie::Signal::SIGTERM),
            (libc::SIGKILL, reverie::Signal::SIGKILL),
            (libc::SIGTRAP, reverie::Signal::SIGTRAP),
        ] {
            // without a core dump
            let raw = std::process::ExitStatus::from_raw(signum);
            assert_eq!(
                process_status(raw),
                ExitStatus::Signaled(signal, false),
                "death by signal {signum} must be reported as Signaled, not Exited"
            );
            // with the core-dump flag set (bit 0x80 of the wait status)
            let raw = std::process::ExitStatus::from_raw(signum | 0x80);
            assert_eq!(
                process_status(raw),
                ExitStatus::Signaled(signal, true),
                "the core-dump flag for signal {signum} must survive the conversion"
            );
        }
    }
}
