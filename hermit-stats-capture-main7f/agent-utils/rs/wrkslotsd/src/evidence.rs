//! Typed, externally collected evidence for read-only shadow decisions.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{is_sha256, load_typed_json, Digested};
use crate::replay::validate_name;
use crate::ObserverError;

/// The observed state of the exact recorded transient scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScopeState {
    Live,
    Dead,
    Unknown,
}

/// What a fresh process census found below a slot root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProcessUse {
    Unused,
    InUse,
    Unknown,
}

/// Whether an operation journal can still own the slot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum JournalState {
    Absent,
    Present,
    Unknown,
}

/// The state of the PID/start-time pair recorded for the task scope leader.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LeaderState {
    Same,
    Reused,
    Absent,
    Unknown,
}

/// A systemd scope identity copied from the active event record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeIdentity {
    pub(crate) unit: String,
    pub(crate) invocation_id: String,
    pub(crate) cgroup_path: String,
    pub(crate) boot_id: String,
    pub(crate) leader_pid: u64,
    pub(crate) leader_start_ticks: u64,
    pub(crate) verification: String,
}

impl ScopeIdentity {
    pub(crate) fn validate(&self, label: &str) -> Result<(), ObserverError> {
        if !self.unit.ends_with(".scope")
            || self.unit.contains('/')
            || self.unit.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || is_unicode_format_control(character)
            })
        {
            return Err(ObserverError::invalid(format!(
                "{label}.unit is not a task-scoped systemd unit"
            )));
        }
        if self.invocation_id.len() != 32
            || !self
                .invocation_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ObserverError::invalid(format!(
                "{label}.invocation_id is not a canonical systemd invocation ID"
            )));
        }
        if !self.cgroup_path.starts_with('/')
            || !self.cgroup_path.ends_with(&format!("/{}", self.unit))
            || self.cgroup_path.contains("/../")
        {
            return Err(ObserverError::invalid(format!(
                "{label}.cgroup_path does not identify its scope unit"
            )));
        }
        if !is_boot_id(&self.boot_id) || self.leader_pid == 0 || self.leader_start_ticks == 0 {
            return Err(ObserverError::invalid(format!(
                "{label} has an invalid boot or process generation"
            )));
        }
        if self.verification != "systemd-runtime-invocation-symlink-v1" {
            return Err(ObserverError::invalid(format!(
                "{label}.verification is not a supported writer verification"
            )));
        }
        Ok(())
    }
}

// Unicode General_Category=Cf as of Unicode 15.0.  These invisible format
// controls include the bidirectional embedding, override, and isolate marks;
// none can safely participate in a human-audited systemd unit identity.
fn is_unicode_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

fn is_boot_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

/// Observation of the exact scope named in one active record.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeEvidence {
    pub(crate) recorded: ScopeIdentity,
    pub(crate) state: ScopeState,
    pub(crate) leader_state: LeaderState,
}

/// Stable identity and path-safety observations for one checkout root.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckoutEvidence {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) mount_id: u64,
    pub(crate) directory: bool,
    pub(crate) symlink_free: bool,
    pub(crate) mount_stable: bool,
}

/// Evidence bound to one active slot generation and record digest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SlotEvidence {
    pub(crate) slot: String,
    pub(crate) generation: u64,
    pub(crate) active_record_sha256: String,
    pub(crate) scope: Option<ScopeEvidence>,
    pub(crate) checkouts: Vec<CheckoutEvidence>,
    pub(crate) journal: JournalState,
    pub(crate) process_use: ProcessUse,
    pub(crate) reclaimable_bytes: u64,
}

/// One captured census used to evaluate every active row in an index rebuild.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceBundle {
    pub(crate) schema: u64,
    pub(crate) machine: String,
    /// Exact event-chain tip against which the census was collected.
    pub(crate) event_tip_sha256: String,
    /// Start of the bounded process/filesystem census window.
    pub(crate) census_started_at: String,
    /// End of the bounded process/filesystem census window.
    pub(crate) observed_at: String,
    pub(crate) boot_id: String,
    pub(crate) slots: Vec<SlotEvidence>,
}

impl EvidenceBundle {
    pub(crate) fn load(path: &Path) -> Result<Digested<Self>, ObserverError> {
        let loaded: Digested<Self> = load_typed_json(path, "shadow policy evidence")?;
        loaded.value.validate()?;
        Ok(loaded)
    }

    pub(crate) fn validate(&self) -> Result<(), ObserverError> {
        if self.schema != 1 {
            return Err(ObserverError::invalid(format!(
                "unsupported shadow policy evidence schema {}",
                self.schema
            )));
        }
        validate_name(&self.machine, "shadow evidence machine")?;
        if !is_sha256(&self.event_tip_sha256) {
            return Err(ObserverError::invalid(
                "shadow evidence event_tip_sha256 must be a lowercase SHA-256",
            ));
        }
        if !is_boot_id(&self.boot_id) {
            return Err(ObserverError::invalid(
                "shadow evidence boot_id must be a canonical lowercase UUID",
            ));
        }
        chrono::DateTime::parse_from_rfc3339(&self.census_started_at).map_err(|_| {
            ObserverError::invalid(
                "shadow evidence census_started_at must be an RFC 3339 timestamp",
            )
        })?;
        chrono::DateTime::parse_from_rfc3339(&self.observed_at).map_err(|_| {
            ObserverError::invalid("shadow evidence observed_at must be an RFC 3339 timestamp")
        })?;
        let mut slots = BTreeSet::new();
        for slot in &self.slots {
            validate_name(&slot.slot, "shadow evidence slot")?;
            if !slots.insert(slot.slot.as_str()) {
                return Err(ObserverError::invalid(format!(
                    "duplicate shadow evidence for slot {}",
                    slot.slot
                )));
            }
            if slot.generation == 0 || !is_sha256(&slot.active_record_sha256) {
                return Err(ObserverError::invalid(format!(
                    "shadow evidence for {} has an invalid generation or record digest",
                    slot.slot
                )));
            }
            if let Some(scope) = &slot.scope {
                scope
                    .recorded
                    .validate(&format!("shadow evidence {} scope", slot.slot))?;
            }
            let mut checkouts = BTreeSet::new();
            for checkout in &slot.checkouts {
                validate_name(&checkout.name, "shadow evidence checkout")?;
                if checkout.path.is_empty()
                    || checkout.device == 0
                    || checkout.inode == 0
                    || checkout.mount_id == 0
                    || !checkouts.insert(checkout.name.as_str())
                {
                    return Err(ObserverError::invalid(format!(
                        "shadow evidence for {} has an invalid checkout identity",
                        slot.slot
                    )));
                }
            }
        }
        Ok(())
    }
}
