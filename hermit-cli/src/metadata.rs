/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::path::PathBuf;

use detcore::BlockingMode;
use reverie::process::Command;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// Hermit record version. Recorded as part of hermit-record, hermit-replay
/// will check this version and will fail if hermit-record version is newer.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Deserialize,
    Serialize
)]
#[repr(transparent)]
pub struct RecordVersion(u32);
impl RecordVersion {
    /// Check if the recorder/replayer version is compatible with a given
    /// recording (trace).
    pub fn compatible_with(&self, other: &RecordVersion) -> bool {
        self == other
    }
}

/// hermit record/replay version.
// NB: Increase the version number when there are breaking changes, i.e.:
// when new syscalls or event schemas are added.
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x10d);

/// Metadata associated with the recording. This is serialized as a JSON file.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    /// The real path to the program.
    pub exe: PathBuf,
    /// The name of the program.
    pub program: String,
    /// The first argument passed to the program.
    pub arg0: String,
    /// Program arguments (not including arg0).
    pub args: Vec<String>,
    /// The working directory of the program.
    pub current_dir: PathBuf,
    /// The hostname in the UTS namespace used by the program.
    pub hostname: Option<String>,
    /// The domainname in the UTS namespace used by the program.
    pub domainname: Option<String>,
    /// The environment variables used by the program.
    pub envs: BTreeMap<String, String>,
    /// Hermit record/replay version.
    pub version: RecordVersion,
}

impl Metadata {
    /// Creates a new metadata object, populating it with information about a
    /// command.
    pub fn new(command: &Command) -> Result<Self, Error> {
        let exe = command.find_program()?;

        let program = command.get_program().to_string_lossy().into_owned();
        let arg0 = command.get_arg0().to_string_lossy().into_owned();

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        let envs = command
            .get_captured_envs()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();

        let current_dir = command
            .get_current_dir()
            .map_or_else(|| env::current_dir().unwrap(), ToOwned::to_owned);

        let hostname = command
            .get_hostname()
            .map(|s| s.to_string_lossy().into_owned());
        let domainname = command
            .get_domainname()
            .map(|s| s.to_string_lossy().into_owned());

        Ok(Self {
            exe,
            program,
            arg0,
            args,
            current_dir,
            hostname,
            domainname,
            envs,
            version: RECORD_VERSION,
        })
    }

    /// Constructs a command from the metadata.
    pub fn command(&self) -> Command {
        // NOTE: We bypass the normal $PATH search here by passing in the
        // absolute path to the program directly.
        let mut command = Command::new(&self.exe);
        command.arg0(&self.arg0);
        command.args(&self.args);
        command.env_clear();
        command.envs(&self.envs);
        command.current_dir(&self.current_dir);

        if let Some(hostname) = &self.hostname {
            command.hostname(hostname);
        }

        if let Some(domainname) = &self.domainname {
            command.domainname(domainname);
        }

        command
    }
}

// TODO: Record this in the metadata instead of hardcoding this.
pub fn record_or_replay_config(data: &Path) -> detcore::Config {
    // NOTE: Record and replay should use the exact same detcore
    // configuration. Otherwise, the behavior of the program could diverge
    // during replay.
    //
    // WHY THIS IS NOT `hermit run --strict`, WRITTEN HERE ON PURPOSE.
    //
    // `virtualize_time: false` below is DELIBERATE, and the rationale used to live only
    // in the separate dev-hermit workspace -- not in this repository, beside the code it
    // governs. The cost of that was measured: three agents in one night read this line,
    // searched this repo's source, git log, docs/ and issues, found nothing, and could
    // not tell a design decision from a determinism bug; two coordinators then spent
    // hours treating it as a candidate defect. A decision recorded in a different
    // repository from the code it governs is, in practice, undocumented. Hence this
    // comment. See rrnewton/hermit#2295.
    //
    // WHAT RECORD/REPLAY ACTUALLY GUARANTEES. Replay re-executes a recording against the
    // recorded syscall data, so what must be reproducible is THIS recording's replay --
    // not agreement between two independent recordings. Time is therefore left real: the
    // recording captures what the guest actually observed, and replay returns those
    // recorded values. `hermit record start --verify` records once, replays that
    // recording, and compares the two; its success message says exactly that ("replay
    // matched recording") and does not claim more.
    //
    // WHAT IT DOES NOT GUARANTEE, which is the part that misleads readers. Because time
    // is not virtualized, two INDEPENDENT recordings of the same program observe
    // different clock values. Demonstrated: `hermit run -- date` twice returns the
    // identical virtual epoch, while `hermit record start -- date` twice returns real
    // wall-clock times seconds apart. Replay fidelity says nothing about that, and
    // nothing in either test suite currently compares two independent recordings.
    // So do not read a green `--verify` as cross-recording determinism.
    //
    // This configuration differs from `hermit run --strict` on four of the five
    // properties that define it (see run.rs: only `sequentialize_threads` matches).
    // Any claim that recording is "strict" in that sense is wrong; the `--strict` flag
    // on `hermit record` is accepted and ignored purely for command-line compatibility.
    let default_config: detcore::Config = Default::default();
    let mut config = detcore::Config {
        panic_on_unsupported_syscalls: false,
        exit_on_unsupported_syscall: false,
        shutdown_on_unsupported_syscall: false,
        unsupported_syscall_report_fd: None,
        panic_on_rcb_overshoot: false,
        sequentialize_threads: true,
        runs_post_fork: default_config.runs_post_fork,
        // Record/replay keeps a partial Detcore subscription. Complete coverage
        // of the Determinized classification begins in v0x10a; madvise policy
        // semantics begin in v0x102.
        passthru_opt: true,
        deterministic_io: false,
        virtualize_time: false,
        virtualize_metadata: false,
        virtualize_cpuid: true,
        cpuid_virtualized_by_backend: false,
        backend_supports_madvise: true,
        discover_live_file_metadata: false,
        use_thread_local_clock_reads: false,
        detect_host_clock_futex_timeouts: false,
        syscall_clobbers_virtualized_by_backend: false,
        cancel_killed_thread_rpcs: false,
        backend_reports_physical_process_exits: false,
        backend_serializes_fork_children: false,
        backend_dispatches_thread_tools: true,
        backend_requires_thread_directed_process_signals: false,
        backend_virtualizes_capability_prctls: false,
        backend_defers_vfork_child_registration: false,
        has_uts_namespace: true,
        // The path to the directory where syscalls will be recorded.
        replay_data: Some(data.to_path_buf()),
        clock_multiplier: None,
        epoch: default_config.epoch,
        gdbserver: false,
        gdbserver_port: default_config.gdbserver_port,
        kill_daemons: default_config.kill_daemons,
        max_timeslice: default_config.max_timeslice,
        target_timeslice: default_config.target_timeslice,
        seed: default_config.seed,
        rng_seed: default_config.rng_seed,
        imprecise_timers: false,
        chaos: false,
        sigint_instakill: false,
        warn_non_zero_binds: false,
        sched_heuristic: Default::default(),
        sched_seed: default_config.sched_seed,
        recordreplay_modes: true,
        record_preemptions: false,
        record_preemptions_to: None,
        replay_preemptions_from: None,
        replay_schedule_from: None,
        replay_exhausted_panic: false,
        die_on_desync: true,
        stacktrace_event: Vec::new(),
        stacktrace_signal: None,
        preemption_stacktrace: false,
        preemption_stacktrace_log_file: None,
        stop_after_turn: None,
        stop_after_iter: None,
        debug_externalize_sockets: false,
        debug_futex_mode: BlockingMode::Precise,
        sched_sticky_random_param: 0.0,
        no_rcb_time: false,
        detlog_heap: false,
        detlog_stack: false,
        detlog_regs: false,
        detlog_io_buffers: false,
        detlog_regs_cadence: 1,
        sysinfo_uptime_offset: 120,
        memory: 1024 * 1024 * 1024,
        interrupt_at: vec![],
        happens_before: None,
        fuzz_futexes: false,
        chaos_target_races: false,
        chaos_per_thread_slowdown: false,
        chaos_slowdown_max_factor: 10.0,
        chaos_epoch_length_ns: 0,
        fuzz_seed: None,
    };
    if config.max_timeslice.is_some() && !reverie_ptrace::is_perf_supported() {
        tracing::warn!(
            "Hardware perf counters are not supported on this machine. Records/Replays may randomly fail!"
        );
        config.max_timeslice = None;
    }
    config
}

#[cfg(test)]
mod tests {
    use reverie::Tool;
    use reverie::syscalls::Sysno;

    use super::*;

    #[test]
    fn record_version_requires_an_exact_match() {
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x10c)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x105)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x110)));
    }

    #[test]
    fn record_and_replay_preserve_partial_subscriptions() {
        assert!(record_or_replay_config(Path::new("replay-data")).passthru_opt);
    }

    #[test]
    fn record_and_replay_subscribe_every_determinized_syscall() {
        let config = record_or_replay_config(Path::new("replay-data"));
        let record = <detcore::Detcore<crate::recorder::Recorder> as Tool>::subscriptions(&config);
        let replay = <detcore::Detcore<crate::replayer::Replayer> as Tool>::subscriptions(&config);

        for (phase, subscriptions) in [("record", record), ("replay", replay)] {
            let delivered = subscriptions.iter_syscalls().collect::<Vec<_>>();
            let missing = detcore::all_pinned_syscalls()
                .filter(|sysno| detcore::is_determinized_syscall(*sysno))
                .filter(|sysno| !delivered.contains(sysno))
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "{phase} lets Determinized syscalls bypass Detcore: {}",
                missing
                    .iter()
                    .map(|sysno| sysno.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            assert!(
                delivered.contains(&Sysno::syslog),
                "{phase} must deliver syslog to its deterministic Detcore handler"
            );
            assert!(
                !delivered.contains(&Sysno::chdir),
                "{phase} must leave unlisted PassThrough chdir unsubscribed"
            );
        }
    }

    #[test]
    fn record_version_rejects_pre_complete_determinized_subscription_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x109)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x104)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x102)));
    }
}
