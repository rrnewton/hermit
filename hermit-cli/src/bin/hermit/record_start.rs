/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::num::NonZeroU64;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Args;
use colored::Colorize;
use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::SerializableError;
use hermit::Shebang;
use hermit::verify_receipt::FileIdentity;
use hermit::verify_receipt::STRICT_RECORD_REPLAY_PROFILE_V1;
use hermit::verify_receipt::StrictReceiptBuildInput;
use hermit::verify_receipt::StrictReceiptExpectation;
use hermit::verify_receipt::StrictRunInput;
use hermit::verify_receipt::TypedTermination;
use hermit::verify_receipt::load_and_verify_strict_receipt;
use hermit::verify_receipt::publish_strict_receipt;
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;
use reverie::process::Command;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Mount;
use reverie::process::MountFlags;

use super::container::IdentityGuard;
use super::container::deterministic_container;
use super::global_opts::GlobalOpts;
use super::run::is_elf_file;
use super::run::path_resolution_visits_prefix;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::LogCompareStrictness;
use super::verify::VerificationReport;
use super::verify::compare_two_runs;
use super::verify::setup_double_run;
use super::verify::validate_log_level;
use super::verify::verification_log_level;
use super::verify::write_pending_verification_json;
use super::verify::write_verification_json;

fn receipt_byte_identity(bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "bytes": bytes.len() as u64,
        "sha256": detcore::Digest::new(bytes).to_string(),
    })
}

#[derive(Debug)]
struct E9patchRecordOverlay {
    source: PathBuf,
    target: PathBuf,
}

static TIMEOUT_MESSAGE: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
static TIMEOUT_MESSAGE_LEN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn recording_timeout_handler(_signal: libc::c_int) {
    let len = TIMEOUT_MESSAGE_LEN.load(Ordering::Acquire);
    let message = TIMEOUT_MESSAGE.load(Ordering::Acquire);
    if !message.is_null() && len != 0 {
        // SAFETY: `message` is leaked before the timer is armed, and fcntl(2),
        // write(2), and _exit(2) are all async-signal-safe.
        unsafe {
            // Make stderr non-blocking before writing. If stderr is a pipe or
            // socket whose buffer is full, a blocking write(2) would wedge the
            // handler and never reach _exit, defeating the deadline. A dropped
            // diagnostic is acceptable; a hung timeout is not.
            let flags = libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL);
            if flags != -1 {
                libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            libc::write(libc::STDERR_FILENO, message.cast(), len);
        }
    }

    // Exiting PID 1 tears down the isolated recording namespace and its tracees.
    // SAFETY: _exit(2) is async-signal-safe and does not run Rust destructors.
    unsafe { libc::_exit(124) }
}

struct RecordingDeadline {
    previous_handler: SigAction,
    // Whether SIGALRM was blocked in the inherited mask and must be re-blocked
    // when the deadline is disarmed.
    reblock_sigalrm: bool,
}

fn sigalrm_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGALRM);
    set
}

impl RecordingDeadline {
    fn arm(timeout: Duration) -> Result<Self, Error> {
        let seconds: libc::c_uint = timeout
            .as_secs()
            .try_into()
            .map_err(|_| Error::msg("record timeout exceeds the platform alarm limit"))?;
        let message = Box::leak(
            format!(
                "Error: Recording timed out after {} seconds; the recording container was terminated\n",
                timeout.as_secs()
            )
            .into_boxed_str(),
        );
        TIMEOUT_MESSAGE.store(message.as_mut_ptr(), Ordering::Release);
        TIMEOUT_MESSAGE_LEN.store(message.len(), Ordering::Release);

        let action = SigAction::new(
            SigHandler::Handler(recording_timeout_handler),
            SaFlags::SA_RESETHAND,
            SigSet::empty(),
        );
        // SAFETY: the handler uses only async-signal-safe operations and remains
        // installed until this guard disarms the alarm.
        let previous_handler = unsafe { sigaction(Signal::SIGALRM, &action) }?;

        // A signal mask inherited from our parent may have SIGALRM blocked. If
        // it is, the alarm signal stays perpetually pending and the handler
        // never runs, silently disabling the deadline. Unblock SIGALRM so the
        // alarm is deliverable, and remember to restore the original state.
        let reblock_sigalrm = SigSet::thread_get_mask()
            .map(|mask| mask.contains(Signal::SIGALRM))
            .unwrap_or(false);
        if reblock_sigalrm {
            let _ = sigalrm_set().thread_unblock();
        }

        // SAFETY: the timeout is nonzero and fits c_uint.
        unsafe { libc::alarm(seconds) };

        Ok(Self {
            previous_handler,
            reblock_sigalrm,
        })
    }
}

impl Drop for RecordingDeadline {
    fn drop(&mut self) {
        // SAFETY: disarm the process-local alarm before restoring its handler.
        unsafe {
            libc::alarm(0);
            let _ = sigaction(Signal::SIGALRM, &self.previous_handler);
        }
        // Restore SIGALRM to its inherited blocked state without disturbing the
        // rest of the mask.
        if self.reblock_sigalrm {
            let _ = sigalrm_set().thread_block();
        }
        TIMEOUT_MESSAGE_LEN.store(0, Ordering::Release);
        TIMEOUT_MESSAGE.store(ptr::null_mut(), Ordering::Release);
    }
}

fn with_recording_deadline<T>(
    timeout: Duration,
    record: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    let _deadline = RecordingDeadline::arm(timeout)?;
    record()
}

#[derive(Debug, Args)]
pub struct StartOpts {
    /// Program to run.
    #[clap(value_name = "PROGRAM", required = true)]
    program: Option<PathBuf>,

    /// Enable strict deterministic recording. Recording is already strict; this flag is retained
    /// for command-line compatibility with `hermit run --strict`.
    #[clap(long = "strict")]
    _strict: bool,

    /// Arguments for the program.
    #[clap(value_name = "ARGS")]
    args: Vec<String>,

    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Kill the recording if the guest does not finish within this many seconds.
    #[clap(long, value_name = "SECONDS")]
    record_timeout: Option<NonZeroU64>,

    /// After recording, immediately replays the command to verify that it works.
    /// This is useful for testing purposes where we often want to verify that
    /// recording was successful.
    ///
    /// The recording is deleted if the replay was successful.
    #[clap(long)]
    verify: bool,

    /// With --verify, write the verification verdict to this path. With
    /// --verify-strict this is an authoritative versioned record/replay receipt
    /// with content-addressed raw evidence; validate it through
    /// --verify-receipt and an independently expected source/command identity.
    /// Without --verify-strict the legacy diagnostic is a single JSON line:
    /// `{"verified":bool,"bitwise_parity":bool,
    /// "verdict":"matched"|"diverged","comparison":{"strictness":
    /// "stripped"|"canonical","compare_logs":bool,"log_scope":
    /// "deterministic"|"info"|"full_trace","strip_lines":bool,
    /// "canonicalize_addresses":bool,"full_trace":bool,"exact_remainder":bool,
    /// "stripped_prefixes":[str],"canonicalizations":[str],"ignore_lines":bool,
    /// "skip_commit":bool,"skip_detlog":bool},"guest_exit_code":int|null,
    /// "guest_signal":int|null}`. This is the exit-code-independent verdict
    /// channel: `verified` reflects whether the record and replay runs matched,
    /// regardless of what the guest exited with, so a caller need not (and must
    /// not) infer the verdict from the process exit code. The flattened
    /// `verified` and `bitwise_parity` fields are diagnostics only;
    /// no admission or parity ratchet may authorize from them.
    #[clap(long, requires = "verify", value_name = "PATH")]
    verify_json: Option<PathBuf>,

    /// With --verify, compare the record and replay logs under the CANONICAL
    /// parity policy: strip only the real wall-clock timestamp prefix, canonicalize
    /// host memory addresses to first-appearance ordinals (tolerating an ASLR
    /// shift while still diverging on allocation-order or aliasing changes), and
    /// compare every INFO message's remaining bytes — virtual-time timestamps,
    /// raw syscall argument/result values, counts, sizes, flags — exactly. An
    /// explicit DEBUG/TRACE level remains captured for diagnostics but does not
    /// change the INFO verdict. Without this the
    /// default `--verify` normalizes away numbers, addresses, tmp paths, and
    /// timestamps before comparing, so a "verified" result asserts only stripped
    /// parity, not bitwise identity. A record/replay determinism ratchet keying on
    /// the verdict should set this so it cannot be silently weakened to a stripped
    /// comparison.
    #[clap(long, requires = "verify")]
    verify_strict: bool,

    /// Verify an already-published record/replay strict receipt without running
    /// the guest. The expected identity is rebuilt from this Hermit executable,
    /// PROGRAM/ARGS, the effective record configuration, and
    /// --expected-source-revision; a cached legacy boolean is never consulted.
    #[clap(
        long,
        value_name = "PATH",
        conflicts_with_all = ["verify", "gdbex"],
        requires = "expected_source_revision"
    )]
    verify_receipt: Option<PathBuf>,

    /// Exact clean 40-hex source revision expected by --verify-receipt.
    #[clap(
        long,
        value_name = "SHA",
        requires = "verify_receipt",
        conflicts_with = "verify"
    )]
    expected_source_revision: Option<String>,

    /// After recording, immediately replays the command to verify that it works
    /// With provided gdb command (passed by `-ex`).
    /// This is useful for testing purposes where we often want to verify that
    /// recording was successful with gdbserver enabled.
    ///
    /// The recording is deleted if the replay was successful.
    #[clap(long = "verify-with-gdbex", value_delimiter = ';')]
    gdbex: Vec<String>,
}

impl StartOpts {
    fn program(&self) -> &PathBuf {
        self.program
            .as_ref()
            .expect("Clap requires PROGRAM unless a record management subcommand is selected")
    }

    fn record_timeout(&self) -> Option<Duration> {
        self.record_timeout
            .map(|seconds| Duration::from_secs(seconds.get()))
    }

    fn selected_backend(&self, global: &GlobalOpts) -> Backend {
        global.backend.unwrap_or_default()
    }

    fn runtime_backend(&self, global: &GlobalOpts) -> Backend {
        if self.selected_backend(global) == Backend::E9patch {
            Backend::Ptrace
        } else {
            self.selected_backend(global)
        }
    }

    fn record_command(&self) -> Command {
        let mut command = Command::new(self.program());
        command.args(&self.args);
        command
    }

    fn strict_receipt_guest_binary_path(&self, global: &GlobalOpts) -> Result<PathBuf, Error> {
        if let Some(overlay) = self.prepare_e9patch_overlay(global)? {
            return Ok(overlay.source);
        }
        let resolved = self
            .record_command()
            .find_program()
            .with_context(|| format!("resolving record guest {}", self.program().display()))?;
        fs::canonicalize(&resolved)
            .with_context(|| format!("canonicalizing record guest {}", resolved.display()))
    }

    /// Build the identity expected by the record/replay receipt from the actual
    /// invocation inputs, without consulting any field in a receipt.
    fn strict_receipt_identity_material(
        &self,
        global: &GlobalOpts,
    ) -> Result<(FileIdentity, FileIdentity, Vec<u8>, Vec<u8>), Error> {
        let producer_path = std::env::current_exe().context("locating Hermit producer binary")?;
        let guest_path = self.strict_receipt_guest_binary_path(global)?;
        let producer = FileIdentity::from_path(&producer_path)?;
        let guest = FileIdentity::from_path(&guest_path)?;
        let command = self.record_command();
        let effective_args: Vec<serde_json::Value> = command
            .get_args()
            .map(|argument| receipt_byte_identity(argument.as_bytes()))
            .collect();
        let captured_environment: Vec<(Vec<u8>, u64, String)> = command
            .get_captured_envs()
            .into_iter()
            .map(|(name, value)| {
                let value = value.as_bytes();
                (
                    name.as_bytes().to_vec(),
                    value.len() as u64,
                    detcore::Digest::new(value).to_string(),
                )
            })
            .collect();
        let guest_command = serde_json::to_vec(&serde_json::json!({
            "schema": "hermit-record-guest-command/v1",
            "requested_program_path": receipt_byte_identity(self.program().as_os_str().as_bytes()),
            "effective_guest_program_path": receipt_byte_identity(command.get_program().as_bytes()),
            "arg0": receipt_byte_identity(command.get_arg0().as_bytes()),
            "args": effective_args,
            "captured_environment_value_identities": captured_environment,
            "guest_binary_sha256": &guest.sha256,
        }))?;
        let selected_backend = self.selected_backend(global);
        let runtime_backend = self.runtime_backend(global);
        let capture_level =
            verification_log_level(global.log, LogCompareStrictness::Canonical, false);
        let effective_run_config = serde_json::to_vec(&serde_json::json!({
            "schema": "hermit-record-replay-effective-config/v1",
            "source_revision": option_env!("HERMIT_BUILD_GIT_FULL_SHA").unwrap_or("unknown"),
            "profile": STRICT_RECORD_REPLAY_PROFILE_V1,
            "selected_backend": selected_backend.as_str(),
            "runtime_backend": runtime_backend.as_str(),
            "capture_log_level": capture_level.to_string().to_ascii_lowercase(),
            "record_timeout_seconds": self.record_timeout().map(|timeout| timeout.as_secs()),
            "captured_environment_value_identities": captured_environment,
        }))?;
        Ok((producer, guest, guest_command, effective_run_config))
    }

    fn strict_receipt_expectation(
        &self,
        global: &GlobalOpts,
        source_revision: &str,
    ) -> Result<StrictReceiptExpectation, Error> {
        let (producer, guest, guest_command, effective_run_config) =
            self.strict_receipt_identity_material(global)?;
        let guest_command_sha256 = detcore::Digest::new(&guest_command).to_string();
        Ok(StrictReceiptExpectation {
            source_revision: source_revision.to_owned(),
            profile: STRICT_RECORD_REPLAY_PROFILE_V1.to_owned(),
            producer_binary_sha256: producer.sha256,
            producer_binary_bytes: producer.bytes,
            guest_binary_sha256: guest.sha256,
            guest_binary_bytes: guest.bytes,
            test_id: format!("sha256:{guest_command_sha256}"),
            effective_run_config_sha256: detcore::Digest::new(&effective_run_config).to_string(),
            selected_backend: self.selected_backend(global).as_str().to_owned(),
            runtime_backend: self.runtime_backend(global).as_str().to_owned(),
        })
    }

    fn verify_published_strict_receipt(
        &self,
        global: &GlobalOpts,
        path: &Path,
        source_revision: &str,
    ) -> Result<ExitStatus, Error> {
        let expectation = self.strict_receipt_expectation(global, source_revision)?;
        let decision = load_and_verify_strict_receipt(path, &expectation);
        eprintln!(":: Strict record/replay receipt: {decision:?}");
        if decision.is_qualified() {
            Ok(ExitStatus::Exited(0))
        } else {
            Err(Error::msg(format!(
                "record/replay receipt did not qualify: {decision:?}"
            )))
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-696): Review /proc magic-link rejection for record overlays.
    fn resolve_e9patch_record_target(&self) -> Result<PathBuf, Error> {
        let command = Command::new(self.program());
        let resolved = command.find_program().with_context(|| {
            format!(
                "Could not resolve program {:?} in PATH for e9patch preprocessing",
                self.program()
            )
        })?;
        if path_resolution_visits_prefix(&resolved, Path::new("/proc"))? {
            anyhow::bail!(
                "e9patch cannot safely overlay executable {} because its path resolves through \
                 /proc; use the executable's stable filesystem path",
                self.program().display()
            );
        }
        fs::canonicalize(&resolved).with_context(|| {
            format!(
                "failed to resolve e9patch executable {}",
                resolved.display()
            )
        })
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-696): Review e9patch record preparation and executable overlaying.
    fn prepare_e9patch_overlay(
        &self,
        global: &GlobalOpts,
    ) -> Result<Option<E9patchRecordOverlay>, Error> {
        if global.backend != Some(Backend::E9patch) {
            return Ok(None);
        }

        Backend::Ptrace.ensure_available()?;
        let target = self.resolve_e9patch_record_target()?;
        if !is_elf_file(&target)? {
            eprintln!(
                ":: Backend: e9patch preprocessing + ptrace record runtime; mapped_sites=0; \
                 main_executable=non-ELF; preprocessing=not-applicable"
            );
            return Ok(None);
        }
        if let Some(reason) = hermit::e9patch::unavailable_reason() {
            anyhow::bail!("backend `e9patch` is unavailable: {reason}");
        }

        let prepared = hermit::e9patch::prepare(&target)?;
        let rewrite_cache = if prepared.patched_sites == 0 {
            "not-applicable"
        } else if prepared.rewrite_cache_hit {
            "hit"
        } else {
            "miss"
        };
        eprintln!(
            ":: Backend: e9patch preprocessing + ptrace record runtime; candidate_sites={}; \
             mapped_sites={}; b0_sites={}; instruction_map_cache={:?}; rewrite_cache={}; \
             artifact_sha256={}",
            prepared.candidate_sites,
            prepared.patched_sites,
            prepared.b0_sites,
            prepared.instruction_map_cache_status,
            rewrite_cache,
            prepared.artifact_sha256.as_deref().unwrap_or("none"),
        );

        Ok(
            (prepared.patched_sites != 0).then_some(E9patchRecordOverlay {
                source: prepared.binary,
                target,
            }),
        )
    }

    fn recording_container(
        &self,
        global: &GlobalOpts,
    ) -> Result<(Container, IdentityGuard), Error> {
        let overlay = self.prepare_e9patch_overlay(global)?;
        let (mut container, identity_guard) = deterministic_container()?;
        if let Some(overlay) = overlay {
            container.mount(Mount::bind(&overlay.source, &overlay.target).readonly());
            container.mount(
                Mount::new(overlay.target)
                    .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
            );
        }
        Ok((container, identity_guard))
    }

    /// The `--verify-json` path this invocation will publish a verdict to, if
    /// any. See `RunOpts::verify_json_path`: the stamp must precede the
    /// top-level preflight, not merely `record_verify`'s first statement.
    pub(crate) fn verify_json_path(&self) -> Option<&Path> {
        self.verify.then_some(self.verify_json.as_deref()).flatten()
    }

    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if let Some(receipt) = &self.verify_receipt {
            validate_log_level(global)?;
            self.verify_published_strict_receipt(
                global,
                receipt,
                self.expected_source_revision
                    .as_deref()
                    .expect("clap requires --expected-source-revision"),
            )
        } else if self.verify {
            validate_log_level(global)?;
            self.record_verify(global)
        } else if !self.gdbex.is_empty() {
            self.record_verify_debug(global)
        } else {
            let hermit = HermitData::from(self.data_dir.as_ref());
            let record_timeout = self.record_timeout();

            let (mut container, _identity_guard) = self.recording_container(global)?;

            let recording = match record_timeout {
                Some(timeout) => {
                    let data = hermit.create_recording_dir()?;
                    let data_path = data.path().to_path_buf();
                    let exit_status = container
                        .run(|| {
                            let _guard = global.init_tracing();
                            let mut command = Command::new(self.program());
                            command.args(&self.args);
                            with_recording_deadline(timeout, || {
                                hermit::record_to(command, &data_path)
                            })
                            .map_err(SerializableError::from)
                        })
                        .context("Container exited unexpectedly")??;
                    hermit.commit_recording(data, exit_status)?
                }
                None => container
                    .run(|| {
                        let _guard = global.init_tracing();
                        let mut command = Command::new(self.program());
                        command.args(&self.args);
                        hermit.record(command).map_err(SerializableError::from)
                    })
                    .context("Container exited unexpectedly")??,
            };

            eprintln!(
                "\n{message}:\n\n    {command} {id}\n",
                message = "RECORDING COMPLETE! To replay, run".yellow().bold(),
                command = "hermit replay".blue().bold(),
                id = recording.id.to_string().bold()
            );

            Ok(recording.exit_status)
        }
    }

    /// This is called when `--verify` is passed to the command line.
    fn record_verify(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        // Stamp an explicit no-result BEFORE any fallible work: the record and
        // replay steps below can fail long before a verdict exists, and a reused
        // --verify-json path must never keep showing a previous invocation's
        // green as though it described this one.
        if let Some(path) = &self.verify_json {
            write_pending_verification_json(path)?;
        }
        let strictness = if self.verify_strict {
            LogCompareStrictness::Canonical
        } else {
            LogCompareStrictness::Stripped
        };
        let ((global1, log1), (global2, log2)) =
            setup_double_run(global, "record", "replay", strictness);
        let strict_identity_before = if self.verify_strict && self.verify_json.is_some() {
            Some(self.strict_receipt_identity_material(global)?)
        } else {
            None
        };

        let (mut recording_container, _record_identity_guard) = self.recording_container(global)?;

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let recording = recording_container
            .run(|| {
                let _guard = global1.init_tracing();

                let mut command = Command::new(self.program());
                command.args(&self.args);

                match record_timeout {
                    Some(timeout) => with_recording_deadline(timeout, || {
                        hermit::record_with_output(command, data_dir)
                    }),
                    None => hermit::record_with_output(command, data_dir),
                }
                .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Replay the recording.
        let (mut replay_container, _replay_identity_guard) = deterministic_container()?;
        let replay = replay_container
            .run(|| {
                let _guard = global2.init_tracing();
                hermit::replay_with_output(data_dir).map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        // Preserve both raw logs before compare_two_runs consumes and deletes
        // their TempPaths. Rebind executable, guest, command, and effective
        // configuration after replay so a mid-run mutation cannot be certified.
        let strict_receipt_material = if self.verify_strict && self.verify_json.is_some() {
            let raw_log1 = fs::read(log1.path())
                .with_context(|| format!("reading record log {}", log1.path().display()))?;
            let raw_log2 = fs::read(log2.path())
                .with_context(|| format!("reading replay log {}", log2.path().display()))?;
            let identity_before = strict_identity_before
                .expect("strict receipt identity was captured before recording");
            let identity_after = self.strict_receipt_identity_material(global)?;
            if identity_before != identity_after {
                anyhow::bail!(
                    "record/replay strict verification identity changed during execution"
                );
            }
            let (producer, guest, guest_command, effective_run_config) = identity_before;
            Some((
                raw_log1,
                raw_log2,
                producer,
                guest,
                guest_command,
                effective_run_config,
            ))
        } else {
            None
        };

        let outcome = compare_two_runs(
            ComparedRun {
                output: &recording,
                log: log1.into_temp_path(),
            },
            ComparedRun {
                output: &replay,
                log: log2.into_temp_path(),
            },
            ComparisonOptions {
                success_message: "Success: replay matched recording.",
                failure_message: "Recording output did not match replay output!",
                verbose: false,
                strictness,
                compare_logs: true,
                diagnostic_full_trace: false,
            },
        )?;

        // Emit the machine-readable verdict (if requested) before collapsing the
        // outcome to the historical exit-code convention, so the verdict is
        // recorded whether or not the runs matched and independent of the guest's
        // own exit status.
        if let Some(path) = &self.verify_json {
            if let Some((
                raw_log1,
                raw_log2,
                producer,
                guest,
                guest_command,
                effective_run_config,
            )) = strict_receipt_material
            {
                let legacy = match serde_json::to_value(VerificationReport::from(&outcome))? {
                    serde_json::Value::Object(map) => map.into_iter().collect(),
                    _ => unreachable!("VerificationReport serializes as an object"),
                };
                let decision = publish_strict_receipt(
                    path,
                    StrictReceiptBuildInput {
                        source_revision: option_env!("HERMIT_BUILD_GIT_FULL_SHA")
                            .unwrap_or("unknown"),
                        profile: STRICT_RECORD_REPLAY_PROFILE_V1,
                        producer_binary: producer,
                        guest_binary: guest,
                        guest_command: &guest_command,
                        effective_run_config: &effective_run_config,
                        selected_backend: self.selected_backend(global).as_str(),
                        runtime_backend: self.runtime_backend(global).as_str(),
                        selected_tests: 1,
                        executed_runs: 2,
                        require_detlog_heap: false,
                        require_detlog_stack: false,
                        fail_closed_strict_requested: true,
                        verify_strict_requested: self.verify_strict,
                        left: StrictRunInput {
                            stdout: &recording.stdout,
                            stderr: &recording.stderr,
                            raw_log: &raw_log1,
                            termination: TypedTermination::from(recording.status),
                        },
                        right: StrictRunInput {
                            stdout: &replay.stdout,
                            stderr: &replay.stderr,
                            raw_log: &raw_log2,
                            termination: TypedTermination::from(replay.status),
                        },
                        legacy,
                    },
                )?;
                eprintln!(":: Strict record/replay receipt: {decision:?}");
                if !decision.is_qualified() {
                    return Err(Error::msg(format!(
                        "record/replay produced no qualifying receipt: {decision:?}"
                    )));
                }
            } else {
                write_verification_json(path, &outcome)?;
            }
        }

        outcome.into_exit_status()
    }
    /// This is called when `--verify-with-gdbex` is passed to the command line.
    fn record_verify_debug(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let (mut container, _identity_guard) = self.recording_container(global)?;

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let _result = container
            .run(|| {
                let _guard = global.init_tracing();

                let mut command = Command::new(self.program());
                command.args(&self.args);

                match record_timeout {
                    Some(timeout) => {
                        with_recording_deadline(timeout, || hermit::record_to(command, data_dir))
                    }
                    None => hermit::record_to(command, data_dir),
                }
                .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Find the path to the executable so that GDB can use it to resolve
        // symbols.
        let exe = data_dir.join("exe");
        let real_exe = Shebang::new(&exe).map_or(exe, |s| s.interpreter().into());

        // Not using fixed port (such as 1234) here because this is mainly
        // intended for tests, which could be running in parallel. This could
        // be flakey when port is already in use.
        let gdbserver_port = 16384 + nix::unistd::gettid().as_raw() as u16 % 1024;

        // Run the gdb client outside of the PID namespace. This cannot be done
        // inside of the PID namespace because it would perturb the
        // deterministic PID allocation that is needed for the replay.
        let mut gdb_command = std::process::Command::new("gdb");
        gdb_command
            .arg(real_exe)
            .arg("-quiet")
            .arg("-iex")
            // don't prompt (dialog) when breakpoint symbol doesn't exist.
            .arg("set breakpoint pending on")
            .arg("-ex")
            .arg(format!("target remote :{}", gdbserver_port));
        for ex in &self.gdbex {
            gdb_command.arg("-ex").arg(ex);
        }
        // Make sure gdb always exit.
        gdb_command.arg("-batch");
        gdb_command.arg("--return-child-result");
        let mut gdb_client = gdb_command
            .spawn()
            .context("Failed to run gdb command. Please make sure it is in your $PATH.")?;

        // TODO: For replay, we ought to construct the container from
        // `metadata.json`. That logic belongs in `hermit::replay`, but we have
        // to initialize logging inside the container because it may spawn a
        // thread. If we can guarantee that tracing won't spawn a thread, then
        // that restriction be lifted.
        let (mut container, _identity_guard) = deterministic_container()?;
        let result = container
            .run(|| {
                let _guard = global.init_tracing();
                hermit::replay_with_gdbserver(data_dir, gdbserver_port)
                    .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;
        let _ = gdb_client.wait();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_options(program: &str) -> StartOpts {
        StartOpts {
            program: Some(PathBuf::from(program)),
            _strict: false,
            args: Vec::new(),
            data_dir: None,
            record_timeout: None,
            verify: false,
            verify_json: None,
            verify_strict: false,
            verify_receipt: None,
            expected_source_revision: None,
            gdbex: Vec::new(),
        }
    }

    fn test_global() -> GlobalOpts {
        GlobalOpts {
            log: None,
            log_file: None,
            backend: None,
        }
    }

    // A blocked SIGALRM (e.g. inherited from the parent) would leave the alarm
    // perpetually pending and silently disable the deadline. Arming must unblock
    // SIGALRM, and dropping must restore the prior blocked state without
    // spuriously blocking a signal that started unblocked. A long timeout keeps
    // the process-wide alarm from firing during the test; the guard's Drop
    // cancels it.
    #[test]
    fn recording_deadline_manages_sigalrm_mask() {
        let sigalrm = sigalrm_set();

        // Case 1: SIGALRM starts blocked. Arm unblocks it; drop re-blocks it.
        sigalrm.thread_block().unwrap();
        assert!(SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM));
        {
            let _deadline = RecordingDeadline::arm(Duration::from_secs(3600)).unwrap();
            assert!(
                !SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM),
                "arming the deadline must unblock SIGALRM so the alarm is deliverable"
            );
        }
        assert!(
            SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM),
            "dropping the deadline must restore the inherited blocked state"
        );

        // Case 2: SIGALRM starts unblocked. Arm leaves it unblocked; drop must
        // not spuriously block it.
        sigalrm.thread_unblock().unwrap();
        assert!(!SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM));
        {
            let _deadline = RecordingDeadline::arm(Duration::from_secs(3600)).unwrap();
            assert!(!SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM));
        }
        assert!(
            !SigSet::thread_get_mask().unwrap().contains(Signal::SIGALRM),
            "dropping must not block SIGALRM when it started unblocked"
        );
    }

    #[test]
    fn e9patch_record_target_rejects_proc_magic_links() {
        let options = test_options("/proc/self/exe");
        let error = options.resolve_e9patch_record_target().unwrap_err();
        assert!(
            error.to_string().contains("resolves through /proc"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn production_consumer_rederives_record_receipt_and_rejects_legacy_json() {
        const SOURCE: &str = "0123456789abcdef0123456789abcdef01234567";
        let options = test_options("/usr/bin/true");
        let global = test_global();
        let (producer, guest, guest_command, effective_run_config) =
            options.strict_receipt_identity_material(&global).unwrap();
        let expectation = StrictReceiptExpectation {
            source_revision: SOURCE.to_owned(),
            profile: STRICT_RECORD_REPLAY_PROFILE_V1.to_owned(),
            producer_binary_sha256: producer.sha256.clone(),
            producer_binary_bytes: producer.bytes,
            guest_binary_sha256: guest.sha256.clone(),
            guest_binary_bytes: guest.bytes,
            test_id: format!("sha256:{}", detcore::Digest::new(&guest_command)),
            effective_run_config_sha256: detcore::Digest::new(&effective_run_config).to_string(),
            selected_backend: Backend::Ptrace.as_str().to_owned(),
            runtime_backend: Backend::Ptrace.as_str().to_owned(),
        };
        let log = b"2026-08-06T01:00:00.000001Z INFO detcore: DETLOG [syscall] exit_group(0)\n\
                    2026-08-06T01:00:00.000002Z INFO detcore::scheduler: [sched-step5] >>>>>>>\n\n COMMIT turn 1\n";
        let directory = tempfile::TempDir::new().unwrap();
        let receipt = directory.path().join("record-receipt.json");
        let decision = publish_strict_receipt(
            &receipt,
            StrictReceiptBuildInput {
                source_revision: SOURCE,
                profile: STRICT_RECORD_REPLAY_PROFILE_V1,
                producer_binary: producer,
                guest_binary: guest,
                guest_command: &guest_command,
                effective_run_config: &effective_run_config,
                selected_backend: Backend::Ptrace.as_str(),
                runtime_backend: Backend::Ptrace.as_str(),
                selected_tests: 1,
                executed_runs: 2,
                require_detlog_heap: false,
                require_detlog_stack: false,
                fail_closed_strict_requested: true,
                verify_strict_requested: true,
                left: StrictRunInput {
                    stdout: b"",
                    stderr: b"",
                    raw_log: log,
                    termination: TypedTermination::Exited { code: 0 },
                },
                right: StrictRunInput {
                    stdout: b"",
                    stderr: b"",
                    raw_log: log,
                    termination: TypedTermination::Exited { code: 0 },
                },
                legacy: std::collections::BTreeMap::new(),
            },
        )
        .unwrap();
        assert!(decision.is_qualified());
        assert_eq!(
            options
                .verify_published_strict_receipt(&global, &receipt, SOURCE)
                .unwrap(),
            ExitStatus::Exited(0)
        );

        let legacy = directory.path().join("legacy.json");
        fs::write(
            &legacy,
            br#"{"verified":true,"bitwise_parity":true,"verdict":"matched"}"#,
        )
        .unwrap();
        assert!(
            !load_and_verify_strict_receipt(&legacy, &expectation).is_qualified(),
            "bare legacy JSON must never authorize"
        );
        let mut wrong_source = expectation;
        wrong_source.source_revision = "f".repeat(40);
        assert!(
            !load_and_verify_strict_receipt(&receipt, &wrong_source).is_qualified(),
            "an independently mismatched source must never authorize"
        );
    }
}
