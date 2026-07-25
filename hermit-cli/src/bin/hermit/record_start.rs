/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::AtomicPtr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Args;
use clap::ValueEnum;
use colored::Colorize;
use hermit::Context;
use hermit::DetConfig;
use hermit::Error;
use hermit::HermitData;
use hermit::RecordFeatures;
use hermit::RecordOptions;
use hermit::SerializableError;
use hermit::Shebang;
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;
use reverie::process::Command;
use reverie::process::ExitStatus;

use super::container::default_container;
use super::global_opts::GlobalOpts;
use super::verify::ComparedRun;
use super::verify::ComparisonOptions;
use super::verify::compare_two_runs;
use super::verify::setup_double_run;

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

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, ValueEnum)]
enum RecordPreset {
    #[default]
    Hermit,
    Portable,
    Rr,
}

impl RecordPreset {
    fn record_features(self) -> RecordFeatures {
        match self {
            Self::Hermit => RecordFeatures::default(),
            Self::Portable => RecordFeatures {
                fs: true,
                ..RecordFeatures::default()
            },
            Self::Rr => RecordFeatures::all(),
        }
    }
}

#[derive(Debug, Args)]
pub struct StartOpts {
    /// Program to run.
    #[clap(value_name = "PROGRAM", required = true)]
    program: Option<PathBuf>,

    /// Select the baseline determinize-or-record policy. Individual record flags add to it.
    #[clap(long, value_enum, default_value_t = RecordPreset::Hermit)]
    preset: RecordPreset,

    /// Record host time instead of using Detcore's logical clock.
    #[clap(long)]
    record_time: bool,

    /// Record guest-visible process and thread identifiers.
    #[clap(long)]
    record_pids: bool,

    /// Record Detcore's schedule and replay it exactly.
    #[clap(long)]
    record_sched: bool,

    /// Record host CPUID results instead of using Detcore's stable CPUID table.
    #[clap(long)]
    record_cpuid: bool,

    /// Record host RNG results instead of using Detcore's seeded PRNG.
    #[clap(long)]
    record_rng: bool,

    /// Explicitly select filesystem capture at the record boundary.
    #[clap(long)]
    record_fs: bool,

    /// Record signal delivery ordering in the schedule trace.
    #[clap(long)]
    record_signals: bool,

    /// Record every configurable nondeterminism source.
    #[clap(long)]
    record_all: bool,

    /// Randomize scheduling while recording and capture the chosen schedule for replay.
    #[clap(long)]
    chaos: bool,

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

    fn det_config(&self) -> DetConfig {
        let mut record_features = if self.record_all {
            RecordFeatures::all()
        } else {
            let mut record_features = self.preset.record_features();
            record_features.time |= self.record_time;
            record_features.pids |= self.record_pids;
            record_features.sched |= self.record_sched;
            record_features.cpuid |= self.record_cpuid;
            record_features.rng |= self.record_rng;
            record_features.fs |= self.record_fs;
            record_features.signals |= self.record_signals;
            record_features
        };
        if self.chaos {
            record_features.sched = true;
        }
        DetConfig {
            record_features,
            chaos: self.chaos,
            ..DetConfig::default()
        }
    }

    fn record_options(&self) -> RecordOptions {
        let config = self.det_config();
        RecordOptions {
            record_features: config.record_features,
            chaos: config.chaos,
        }
    }

    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        if self.verify {
            self.record_verify(global)
        } else if !self.gdbex.is_empty() {
            self.record_verify_debug(global)
        } else {
            let hermit = HermitData::from(self.data_dir.as_ref());
            let record_timeout = self.record_timeout();

            let mut container = default_container(true);

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
                                hermit::record_to_with_options(
                                    command,
                                    &data_path,
                                    self.record_options(),
                                )
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
                        hermit
                            .record_with_options(command, self.record_options())
                            .map_err(SerializableError::from)
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
        let ((global1, log1), (global2, log2)) = setup_double_run(global, "record", "replay");

        let mut container = default_container(true);

        eprintln!(":: {}", "Recording...".yellow().bold());

        let temp_data_dir = tempfile::tempdir()?;
        let data_dir = temp_data_dir.path();
        let record_timeout = self.record_timeout();

        let recording = container
            .run(|| {
                let _guard = global1.init_tracing();

                let mut command = Command::new(self.program());
                command.args(&self.args);

                match record_timeout {
                    Some(timeout) => with_recording_deadline(timeout, || {
                        hermit::record_with_output_options(command, data_dir, self.record_options())
                    }),
                    None => {
                        hermit::record_with_output_options(command, data_dir, self.record_options())
                    }
                }
                .map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        eprintln!(":: {}", "Replaying...".yellow().bold());

        // Replay the recording.
        let replay = container
            .run(|| {
                let _guard = global2.init_tracing();
                hermit::replay_with_output(data_dir).map_err(SerializableError::from)
            })
            .context("Container exited unexpectedly")??;

        // Captured schedule replay emits different Detcore bookkeeping from recording it.
        // In that mode, compare the guest-observable output and status only.
        let features = self.record_options().record_features;

        compare_two_runs(
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
                compare_logs: !(features.sched || features.signals),
                verbose: false,
            },
        )
    }
    /// This is called when `--verify-with-gdbex` is passed to the command line.
    fn record_verify_debug(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let mut container = default_container(true);

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
                    Some(timeout) => with_recording_deadline(timeout, || {
                        hermit::record_to_with_options(command, data_dir, self.record_options())
                    }),
                    None => {
                        hermit::record_to_with_options(command, data_dir, self.record_options())
                    }
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
        let mut container = default_container(true);
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

    #[derive(clap::Parser)]
    struct TestStart {
        #[clap(flatten)]
        opts: StartOpts,
    }

    #[test]
    fn record_feature_policy_defaults_to_determinization() {
        let parsed = <TestStart as clap::Parser>::parse_from(["test", "/bin/true"]);
        assert_eq!(
            parsed.opts.det_config().record_features,
            RecordFeatures::default()
        );
    }

    #[test]
    fn record_feature_policy_preserves_individual_choices() {
        let parsed = <TestStart as clap::Parser>::parse_from([
            "test",
            "--record-time",
            "--record-rng",
            "/bin/true",
        ]);
        assert_eq!(
            parsed.opts.det_config().record_features,
            RecordFeatures {
                time: true,
                rng: true,
                ..RecordFeatures::default()
            }
        );
    }

    #[test]
    fn record_all_overrides_individual_feature_selection() {
        let parsed = <TestStart as clap::Parser>::parse_from([
            "test",
            "--record-all",
            "--record-time",
            "/bin/true",
        ]);
        assert_eq!(
            parsed.opts.det_config().record_features,
            RecordFeatures::all()
        );
    }

    #[test]
    fn record_presets_select_expected_boundaries() {
        let hermit =
            <TestStart as clap::Parser>::parse_from(["test", "--preset=hermit", "/bin/true"]);
        assert_eq!(
            hermit.opts.det_config().record_features,
            RecordFeatures::default()
        );

        let portable =
            <TestStart as clap::Parser>::parse_from(["test", "--preset=portable", "/bin/true"]);
        assert_eq!(
            portable.opts.det_config().record_features,
            RecordFeatures {
                fs: true,
                ..RecordFeatures::default()
            }
        );

        let rr = <TestStart as clap::Parser>::parse_from(["test", "--preset=rr", "/bin/true"]);
        assert_eq!(rr.opts.det_config().record_features, RecordFeatures::all());
    }

    #[test]
    fn chaos_forces_schedule_capture_without_changing_other_boundaries() {
        let parsed = <TestStart as clap::Parser>::parse_from(["test", "--chaos", "/bin/true"]);
        let config = parsed.opts.det_config();

        assert!(config.chaos);
        assert_eq!(
            config.record_features,
            RecordFeatures {
                sched: true,
                ..RecordFeatures::default()
            }
        );
    }

    #[test]
    fn chaos_composes_with_presets_individual_and_record_all() {
        let individual = <TestStart as clap::Parser>::parse_from([
            "test",
            "--preset=portable",
            "--record-time",
            "--record-rng",
            "--chaos",
            "/bin/true",
        ]);
        assert_eq!(
            individual.opts.det_config().record_features,
            RecordFeatures {
                time: true,
                sched: true,
                rng: true,
                fs: true,
                ..RecordFeatures::default()
            }
        );

        let rr = <TestStart as clap::Parser>::parse_from([
            "test",
            "--preset=rr",
            "--chaos",
            "/bin/true",
        ]);
        assert!(rr.opts.det_config().chaos);
        assert_eq!(rr.opts.det_config().record_features, RecordFeatures::all());

        let all = <TestStart as clap::Parser>::parse_from([
            "test",
            "--record-all",
            "--chaos",
            "/bin/true",
        ]);
        let config = all.opts.det_config();
        assert!(config.chaos);
        assert_eq!(config.record_features, RecordFeatures::all());
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
}
