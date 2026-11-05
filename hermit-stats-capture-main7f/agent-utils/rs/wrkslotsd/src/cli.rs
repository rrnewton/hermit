//! Command-line interface for rebuilding and reading the observer index.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::index::{read_decision, read_pressure_plan, rebuild_policy_index};
use crate::{read_index, rebuild_index, ReplaySummary};

#[derive(Parser)]
#[command(
    name = "wrkslotsd",
    version,
    about = "Validate slot event history and maintain a disposable read-only observer index",
    long_about = "Validate a hash-linked EVENTS.<machine> directory and materialize a disposable SQLite observer index. Replay covers path-independent record semantics; the shadow-policy JSON is not the originating .wrkslots.yml registry, and configuration-coupled paths, layout defaults, landed refs, Git registrations, and nested-repository discovery remain with Python. Optional typed configuration and caller-supplied captured evidence produce fail-closed, digest-bound diagnostic decisions. This slice does not independently recheck /proc, boot ID, the systemd invocation link, cgroup existence, or TaskGraph claim ownership, so it cannot emit actionable eligibility. Explain and plan reconstruct and self-consistency-check decisions and reject evidence that has aged out; the disposable database is not authenticated against a coordinated same-UID rewrite, live EVENTS, or repositories absent from its inputs. Initial publication requires a filesystem that supports renameat2(RENAME_NOREPLACE). This migration slice has no daemon, socket, lease, cleanup, rescue, deletion, apply, or repository mutation capability.",
    after_help = "Examples:\n  wrkslotsd rebuild --events-dir /srv/control/EVENTS.node-a --index /var/tmp/slot-observer.sqlite\n  wrkslotsd rebuild --events-dir /srv/control/EVENTS.node-a --index /var/tmp/slot-observer.sqlite --config policy.json --evidence evidence.json\n  wrkslotsd explain --index /var/tmp/slot-observer.sqlite --slot worker-a\n  wrkslotsd plan --index /var/tmp/slot-observer.sqlite --target-bytes 107374182400"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Strictly replay an event log and atomically replace the derived index
    Rebuild {
        /// Real EVENTS.<machine> directory to validate; it is never modified
        #[arg(long, value_name = "DIR")]
        events_dir: PathBuf,
        /// Disposable SQLite index path outside the event directory
        #[arg(long, value_name = "FILE")]
        index: PathBuf,
        /// Shadow-policy JSON (not .wrkslots.yml); requires --evidence
        #[arg(long, value_name = "FILE", requires = "evidence")]
        config: Option<PathBuf>,
        /// Caller-supplied captured census JSON; requires --config
        #[arg(long, value_name = "FILE", requires = "config")]
        evidence: Option<PathBuf>,
    },
    /// Read and cross-check summary counters from an existing derived index
    Status {
        /// Existing disposable SQLite index
        #[arg(long, value_name = "FILE")]
        index: PathBuf,
    },
    /// Explain one reconstructed shadow decision; never changes source or index state
    Explain {
        /// Existing disposable SQLite index
        #[arg(long, value_name = "FILE")]
        index: PathBuf,
        /// Active or pending-operation slot name
        #[arg(long)]
        slot: String,
    },
    /// Partition reconstructed decisions into a bounded, non-executing pressure report
    Plan {
        /// Existing disposable SQLite index
        #[arg(long, value_name = "FILE")]
        index: PathBuf,
        /// Future eligibility target; currently zero rows can pass the rollout gates
        #[arg(long, default_value_t = 0)]
        target_bytes: u64,
        /// Optional stricter future-candidate count; indexed configuration remains the ceiling
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Serialize)]
struct CliSummary<'a> {
    machine: &'a str,
    replay_count: u64,
    tip_sha256: &'a str,
    // Revisions legitimately span all of u64. JSON strings prevent consumers
    // backed by binary64 numbers from silently rounding them.
    active_revision: String,
    active_count: u64,
    archive_revision: String,
    archive_count: u64,
}

impl<'a> From<&'a ReplaySummary> for CliSummary<'a> {
    fn from(summary: &'a ReplaySummary) -> Self {
        Self {
            machine: &summary.machine,
            replay_count: summary.replay_count,
            tip_sha256: &summary.tip_sha256,
            active_revision: summary.active_revision.to_string(),
            active_count: summary.active_count,
            archive_revision: summary.archive_revision.to_string(),
            archive_count: summary.archive_count,
        }
    }
}

/// Parse process arguments, print one JSON summary, and return a process code.
pub fn run() -> u8 {
    let command = Cli::parse().command;
    let result: Result<serde_json::Value, crate::ObserverError> = match command {
        Command::Rebuild {
            events_dir,
            index,
            config,
            evidence,
        } => match (config, evidence) {
            (Some(config), Some(evidence)) => {
                rebuild_policy_index(&events_dir, &index, &config, &evidence)
            }
            (None, None) => rebuild_index(&events_dir, &index),
            _ => unreachable!("clap enforces paired policy inputs"),
        }
        .and_then(|summary| encode_value(&CliSummary::from(&summary))),
        Command::Status { index } => {
            read_index(&index).and_then(|summary| encode_value(&CliSummary::from(&summary)))
        }
        Command::Explain { index, slot } => {
            read_decision(&index, &slot).and_then(|decision| encode_value(&decision))
        }
        Command::Plan {
            index,
            target_bytes,
            limit,
        } => read_pressure_plan(&index, target_bytes, limit).and_then(|plan| encode_value(&plan)),
    };
    match result {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("wrkslotsd: {error}");
            1
        }
    }
}

fn encode_value(value: &impl Serialize) -> Result<serde_json::Value, crate::ObserverError> {
    serde_json::to_value(value)
        .map_err(|error| crate::ObserverError::with_source("cannot encode command output", error))
}
