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
use detcore::RecordFeatures;
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
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x104);

const SCHEDULE_NAME: &str = "schedule.json";

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
    /// Sources captured by this recording instead of determinized by Detcore.
    #[serde(default)]
    pub record_features: RecordFeatures,
}

impl Metadata {
    /// Creates a new metadata object, populating it with information about a
    /// command.
    pub fn new(command: &Command, record_features: RecordFeatures) -> Result<Self, Error> {
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
            record_features,
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

#[derive(Clone, Copy)]
pub enum RecordReplayMode {
    Record,
    Replay,
}

pub fn record_or_replay_config(
    data: &Path,
    record_features: RecordFeatures,
    mode: RecordReplayMode,
) -> detcore::Config {
    // NOTE: Record and replay should use the exact same detcore
    // configuration. Otherwise, the behavior of the program could diverge
    // during replay.
    let default_config: detcore::Config = Default::default();
    let record_schedule = record_features.sched || record_features.signals;
    let schedule_path = data.join(SCHEDULE_NAME);
    let mut config = detcore::Config {
        panic_on_unsupported_syscalls: false,
        sequentialize_threads: true,
        runs_post_fork: default_config.runs_post_fork,
        // Record/replay keeps partial Detcore subscription; madvise policy semantics
        // begin in v0x102.
        passthru_opt: true,
        deterministic_io: false,
        virtualize_time: !record_features.time,
        // Record/replay needs real descriptor metadata for file-backed mmap and
        // loader bootstrap. The filesystem stream remains captured in both modes.
        virtualize_metadata: false,
        virtualize_cpuid: !record_features.cpuid,
        cpuid_virtualized_by_backend: false,
        backend_supports_madvise: true,
        has_uts_namespace: true,
        // The path to the directory where syscalls will be recorded.
        replay_data: Some(data.to_path_buf()),
        clock_multiplier: None,
        epoch: default_config.epoch,
        gdbserver: false,
        gdbserver_port: default_config.gdbserver_port,
        kill_daemons: default_config.kill_daemons,
        preemption_timeout: default_config.preemption_timeout,
        seed: default_config.seed,
        rng_seed: default_config.rng_seed,
        imprecise_timers: false,
        chaos: false,
        sigint_instakill: false,
        warn_non_zero_binds: false,
        sched_heuristic: Default::default(),
        sched_seed: default_config.sched_seed,
        recordreplay_modes: true,
        record_features,
        record_preemptions: record_schedule && matches!(mode, RecordReplayMode::Record),
        record_preemptions_to: (record_schedule && matches!(mode, RecordReplayMode::Record))
            .then_some(schedule_path.clone()),
        replay_preemptions_from: None,
        replay_schedule_from: (record_schedule && matches!(mode, RecordReplayMode::Replay))
            .then_some(schedule_path),
        replay_exhausted_panic: record_schedule && matches!(mode, RecordReplayMode::Replay),
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
        sysinfo_uptime_offset: 120,
        memory: 1024 * 1024 * 1024,
        interrupt_at: vec![],
        fuzz_futexes: false,
        chaos_target_races: false,
        fuzz_seed: None,
    };
    if config.preemption_timeout.is_some() && !reverie_ptrace::is_perf_supported() {
        tracing::warn!(
            "Hardware perf counters are not supported on this machine. Records/Replays may randomly fail!"
        );
        config.preemption_timeout = None;
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_version_requires_an_exact_match() {
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x103)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x105)));
    }

    #[test]
    fn record_features_default_when_reading_legacy_metadata() {
        let command = Command::new("/bin/true");
        let metadata = Metadata::new(&command, RecordFeatures::all()).unwrap();
        let mut value = serde_json::to_value(metadata).unwrap();
        value.as_object_mut().unwrap().remove("record_features");
        value["version"] = serde_json::json!(0x103);

        let parsed: Metadata = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.version, RecordVersion(0x103));
        assert_eq!(parsed.record_features, RecordFeatures::default());
    }

    #[test]
    fn record_and_replay_preserve_partial_subscriptions() {
        for mode in [RecordReplayMode::Record, RecordReplayMode::Replay] {
            assert!(
                record_or_replay_config(Path::new("replay-data"), RecordFeatures::default(), mode)
                    .passthru_opt
            );
        }
    }

    #[test]
    fn default_record_policy_determinizes_internal_sources() {
        let config = record_or_replay_config(
            Path::new("replay-data"),
            RecordFeatures::default(),
            RecordReplayMode::Record,
        );
        assert!(config.virtualize_time);
        assert!(config.virtualize_cpuid);
        assert!(!config.virtualize_metadata);
        assert!(!config.record_preemptions);
        assert_eq!(config.record_features, RecordFeatures::default());
    }

    #[test]
    fn record_all_captures_sources_and_replays_the_schedule() {
        let features = RecordFeatures::all();
        let record =
            record_or_replay_config(Path::new("replay-data"), features, RecordReplayMode::Record);
        assert!(!record.virtualize_time);
        assert!(!record.virtualize_cpuid);
        assert!(!record.virtualize_metadata);
        assert_eq!(
            record.record_preemptions_to,
            Some(PathBuf::from("replay-data/schedule.json"))
        );

        let replay =
            record_or_replay_config(Path::new("replay-data"), features, RecordReplayMode::Replay);
        assert!(replay.replay_exhausted_panic);
        assert!(replay.die_on_desync);
        assert_eq!(
            replay.replay_schedule_from,
            Some(PathBuf::from("replay-data/schedule.json"))
        );
    }

    #[test]
    fn record_version_rejects_pre_madvise_policy_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x102)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x103)));
    }
}
