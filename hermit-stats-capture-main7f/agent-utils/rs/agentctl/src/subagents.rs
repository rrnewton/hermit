//! Named, long-lived interactive agents sharing a Herdr workspace.
//!
//! Herdr owns terminals and harness processes. This layer owns durable names,
//! launch intent, queue routing, snapshots, and conservative tab teardown.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{
    DirBuilderExt, FileExt as UnixFileExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::agent::{self, AgentApi, AgentError, DrainOptions, QueueResult, Target};
use crate::client::{
    muse_auto_review_idle_composer, muse_prompt_in_composer, muse_prompt_is_exact_composer,
    muse_prompt_transcript_count, muse_startup_metadata, muse_trust_prompt,
    muse_verified_pane_status, AgentIdentity, AgentPaneInfo, CustomProcessIdentity, HerdrClient,
    Pane, PaneShellProof, ProcessLiveness,
};
use crate::error::{AdapterError, AdapterErrorKind};
use crate::submission::{GuardedInput, GuardedPrompt, Submission, SubmitTimeouts};

mod cloud;
mod nested;
mod revive;
mod stop_advice;
mod turn_runner;

pub(crate) use cloud::{
    validate_create_arguments as validate_cloud_create_arguments, CLOUD_DRIVERS,
};
pub use cloud::{CloudLaunch, CloudTools};
pub(crate) use stop_advice::{stop_reason, StopContext};
pub use stop_advice::{RecoveryAction, StopFailure};

const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";
const MAX_AGENT_RECORD_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

/// The herdr read source of the chat service's reads of its agent's pane: the reads that recover
/// replies when the service starts, at each reconciliation, when the pane settles idle or done,
/// and after an output event names a reply ID the service does not know; the read it repeats
/// while a reply alias keeps an output pattern matched; and the read just before a request's
/// prompt is written. It is the screen of a pane that herdr reports keeps no scrollback, and
/// otherwise the recent lines with wrapped rows joined, or the recent rows when herdr returns no
/// such lines. Claude Code redraws one screen in place and keeps none; a recent read of it while
/// it is idle can return more than one screen, joined from what it drew at different times, and
/// scrolls the pane's view as it reads. The service's other capture path, its
/// `pane.output_matched` subscriptions, asks for `recent_unwrapped` lines, as many as these reads
/// do, which herdr serves as passive reads that never scroll the view, so for a pane with
/// scrollback a read here sees the lines herdr matches the subscriptions against. `agentctl chat
/// tick` makes the same choice by the same test, [`AgentPaneInfo::keeps_no_scrollback`], in a
/// second copy, [`agent::read_capture`].
pub(crate) fn chat_capture_source(info: &AgentPaneInfo) -> &'static str {
    if info.keeps_no_scrollback() {
        "visible"
    } else {
        "recent-unwrapped"
    }
}

fn rename_directory_noreplace_at(
    source_parent: &File,
    source_name: &str,
    destination_parent: &File,
    destination_name: &str,
) -> Result<()> {
    let source_bytes = CString::new(source_name.as_bytes())
        .map_err(|_| fail("agent archive source path contains NUL"))?;
    let destination_bytes = CString::new(destination_name.as_bytes())
        .map_err(|_| fail("agent archive destination path contains NUL"))?;
    let result = unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source_bytes.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_bytes.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EEXIST) => Err(fail(format!(
            "refusing to replace existing agent archive {destination_name}",
        ))),
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) => Err(fail(
            "cannot archive agent: filesystem lacks atomic no-replace rename support",
        )),
        _ => Err(fail(format!(
            "cannot archive agent {source_name} as {destination_name}: {error}",
        ))),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InstalledArtifact {
    device: u64,
    inode: u64,
    size: u64,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactInstallState {
    NotInstalled,
    Installed(InstalledArtifact),
    Uncertain,
}

#[derive(Debug)]
struct ArtifactWriteError {
    error: Box<AgentError>,
    state: ArtifactInstallState,
}

impl std::fmt::Display for ArtifactWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl From<AgentError> for ArtifactWriteError {
    fn from(error: AgentError) -> Self {
        Self {
            error: Box::new(error),
            state: ArtifactInstallState::NotInstalled,
        }
    }
}

type ArtifactWriteResult = std::result::Result<InstalledArtifact, ArtifactWriteError>;

fn atomic_replace_bytes(
    pinned: &PinnedAgentDirectory,
    name: &str,
    content: &[u8],
) -> ArtifactWriteResult {
    atomic_replace_bytes_with(
        pinned,
        name,
        content,
        (
            |_pinned, _temporary| Ok(()),
            |_pinned, _name, _temporary| Ok(()),
        ),
    )
}

fn atomic_replace_bytes_with<AfterWrite, AfterRename>(
    pinned: &PinnedAgentDirectory,
    name: &str,
    content: &[u8],
    hooks: (AfterWrite, AfterRename),
) -> ArtifactWriteResult
where
    AfterWrite: FnOnce(&PinnedAgentDirectory, &str) -> Result<()>,
    AfterRename: FnOnce(&PinnedAgentDirectory, &str, &str) -> Result<()>,
{
    let (after_write, after_rename) = hooks;
    if !matches!(name, "agent.json" | "output.json") && !revive::artifact_name_allowed(pinned, name)
    {
        return Err(fail("unsupported pinned registry artifact name").into());
    }
    let limit = if name == "output.json" {
        MAX_SNAPSHOT_BYTES
    } else {
        MAX_AGENT_RECORD_BYTES
    };
    if content.len() > limit {
        return Err(fail(format!("refusing {name} larger than {limit} bytes")).into());
    }
    let name_c = CString::new(name).map_err(|_| fail("registry artifact name contains NUL"))?;
    let mut selected = None;
    for attempt in 0_u32..128 {
        let candidate = format!(
            ".{name}-recovery-{}-{}-{attempt}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| fail(error.to_string()))?
                .as_nanos()
        );
        let candidate_c = CString::new(candidate.as_bytes())
            .map_err(|_| fail("registry staging name contains NUL"))?;
        let descriptor = unsafe {
            libc::openat(
                pinned.file.as_raw_fd(),
                candidate_c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if descriptor >= 0 {
            selected = Some((candidate_c, unsafe { File::from_raw_fd(descriptor) }));
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(fail(format!("cannot stage output snapshot: {error}")).into());
        }
    }
    let (temporary, mut file) = selected.ok_or_else(|| {
        ArtifactWriteError::from(fail("cannot reserve private output snapshot staging file"))
    })?;
    let temporary_name = temporary
        .to_str()
        .expect("constructed registry staging name is UTF-8");
    let uid = unsafe { libc::getuid() };
    let mut created_identity = None;
    let mut rename_attempted = false;
    let mut renamed = false;
    let mut installed_verified = false;
    let write_result = (|| -> Result<()> {
        let created = file
            .metadata()
            .map_err(|error| fail(format!("cannot inspect registry staging file: {error}")))?;
        if !created.is_file()
            || created.uid() != uid
            || created.permissions().mode() & 0o077 != 0
            || created.nlink() != 1
        {
            return Err(fail("unsafe registry artifact staging file"));
        }
        let identity = (created.dev(), created.ino());
        created_identity = Some(identity);
        file.write_all(content)
            .map_err(|error| fail(format!("cannot write registry staging file: {error}")))?;
        file.sync_all()
            .map_err(|error| fail(format!("cannot sync registry staging file: {error}")))?;
        after_write(pinned, temporary_name)?;
        let held = file
            .metadata()
            .map_err(|error| fail(format!("cannot reinspect registry staging file: {error}")))?;
        let mut held_content = vec![0_u8; content.len() + 1];
        let held_length = file
            .read_at(&mut held_content, 0)
            .map_err(|error| fail(format!("cannot reread registry staging file: {error}")))?;
        let staged = ManagedAgents::<HerdrClient>::open_optional_pinned_file(
            pinned,
            temporary_name,
            libc::O_RDONLY,
        )?
        .ok_or_else(|| fail("registry artifact staging name disappeared before installation"))?;
        let staged_metadata = staged.metadata().map_err(|error| {
            fail(format!(
                "cannot inspect registry artifact staging name: {error}"
            ))
        })?;
        if !held.is_file()
            || !staged_metadata.is_file()
            || held.uid() != uid
            || staged_metadata.uid() != uid
            || held.permissions().mode() & 0o077 != 0
            || staged_metadata.permissions().mode() & 0o077 != 0
            || held.nlink() != 1
            || staged_metadata.nlink() != 1
            || held.len() != content.len() as u64
            || staged_metadata.len() != content.len() as u64
            || held_length != content.len()
            || &held_content[..held_length] != content
            || (held.dev(), held.ino()) != identity
            || (staged_metadata.dev(), staged_metadata.ino()) != identity
        {
            return Err(fail(format!(
                "registry artifact staging generation changed before installing {name}"
            )));
        }
        rename_attempted = true;
        let rename_result = unsafe {
            libc::renameat(
                pinned.file.as_raw_fd(),
                temporary.as_ptr(),
                pinned.file.as_raw_fd(),
                name_c.as_ptr(),
            )
        };
        if rename_result != 0 {
            return Err(fail(format!(
                "cannot install pinned registry artifact: {}",
                std::io::Error::last_os_error()
            )));
        }
        // The staging name no longer belongs to this operation. Never remove
        // a new entry that appears there after the rename.
        renamed = true;
        after_rename(pinned, name, temporary_name)?;
        let installed =
            ManagedAgents::<HerdrClient>::open_pinned_file(pinned, name, libc::O_RDONLY)?;
        let installed_metadata = installed.metadata().map_err(|error| {
            fail(format!(
                "cannot inspect installed registry artifact: {error}"
            ))
        })?;
        let held = file.metadata().map_err(|error| {
            fail(format!(
                "cannot reinspect installed registry artifact: {error}"
            ))
        })?;
        let mut held_content = vec![0_u8; content.len() + 1];
        let held_length = file.read_at(&mut held_content, 0).map_err(|error| {
            fail(format!(
                "cannot reread installed registry artifact: {error}"
            ))
        })?;
        if !installed_metadata.is_file()
            || installed_metadata.uid() != uid
            || installed_metadata.permissions().mode() & 0o077 != 0
            || installed_metadata.nlink() != 1
            || installed_metadata.len() != content.len() as u64
            || held_length != content.len()
            || &held_content[..held_length] != content
            || (installed_metadata.dev(), installed_metadata.ino()) != identity
            || (held.dev(), held.ino(), held.len())
                != (identity.0, identity.1, content.len() as u64)
        {
            return Err(fail(format!(
                "installed registry artifact {name} was not the staged generation"
            )));
        }
        installed_verified = true;
        pinned
            .file
            .sync_all()
            .map_err(|error| fail(format!("cannot sync installed registry artifact: {error}")))?;
        Ok(())
    })();
    let mut uncertain = renamed && !installed_verified;
    if !renamed && rename_attempted {
        let mut held_content = vec![0_u8; content.len() + 1];
        let held_content_exact = file
            .read_at(&mut held_content, 0)
            .is_ok_and(|length| length == content.len() && &held_content[..length] == content);
        let held_exact = file.metadata().is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.uid() == uid
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.nlink() == 1
                && metadata.len() == content.len() as u64
                && held_content_exact
                && Some((metadata.dev(), metadata.ino())) == created_identity
        });
        let staged_exact = ManagedAgents::<HerdrClient>::open_optional_pinned_file(
            pinned,
            temporary_name,
            libc::O_RDONLY,
        )
        .ok()
        .flatten()
        .and_then(|staged| staged.metadata().ok())
        .is_some_and(|metadata| {
            metadata.is_file()
                && metadata.uid() == uid
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.nlink() == 1
                && Some((metadata.dev(), metadata.ino())) == created_identity
        });
        let installed_exact =
            ManagedAgents::<HerdrClient>::open_optional_pinned_file(pinned, name, libc::O_RDONLY)
                .ok()
                .flatten()
                .and_then(|installed| installed.metadata().ok())
                .is_some_and(|metadata| {
                    metadata.is_file()
                        && metadata.uid() == uid
                        && metadata.permissions().mode() & 0o077 == 0
                        && metadata.nlink() == 1
                        && metadata.len() == content.len() as u64
                        && Some((metadata.dev(), metadata.ino())) == created_identity
                });
        if held_exact && installed_exact && !staged_exact {
            renamed = true;
            installed_verified = true;
        } else if !(held_exact && staged_exact) {
            uncertain = true;
        }
    }
    let cleanup = if renamed || uncertain {
        Ok(())
    } else {
        match ManagedAgents::<HerdrClient>::open_optional_pinned_file(
        pinned,
        temporary_name,
        libc::O_RDONLY,
    ) {
        Ok(None) => Ok(()),
        Ok(Some(staged)) => match staged.metadata() {
            Ok(metadata)
                if metadata.is_file()
                    && metadata.uid() == uid
                    && metadata.permissions().mode() & 0o077 == 0
                    && metadata.nlink() == 1
                    && Some((metadata.dev(), metadata.ino())) == created_identity =>
            {
                let result = unsafe {
                    libc::unlinkat(pinned.file.as_raw_fd(), temporary.as_ptr(), 0)
                };
                if result == 0 {
                    Ok(())
                } else {
                    Err(fail(format!(
                        "cannot remove owned registry staging file: {}",
                        std::io::Error::last_os_error()
                    )))
                }
            }
            Ok(_) => Err(fail(
                "registry staging name no longer denotes the owned file; replacement was preserved",
            )),
            Err(error) => Err(fail(format!(
                "cannot inspect registry staging file during cleanup: {error}"
            ))),
        },
        Err(error) => Err(error),
        }
    };
    // A same-uid process ignoring the cooperative registry lock can still race
    // the final identity check above and unlinkat.
    let error = match (write_result, cleanup) {
        (Ok(()), Ok(())) => None,
        (Err(error), Ok(())) => Some(error),
        (Ok(()), Err(cleanup)) => Some(cleanup),
        (Err(error), Err(cleanup)) => Some(fail(format!(
            "{error}; registry artifact cleanup failed: {cleanup}"
        ))),
    };
    let artifact = created_identity.map(|(device, inode)| InstalledArtifact {
        device,
        inode,
        size: content.len() as u64,
        digest: Sha256::digest(content).into(),
    });
    match (error, artifact) {
        (None, Some(artifact)) if installed_verified => Ok(artifact),
        (None, _) => Err(ArtifactWriteError {
            error: Box::new(fail(format!(
                "cannot prove installed registry artifact {name}"
            ))),
            state: ArtifactInstallState::Uncertain,
        }),
        (Some(error), Some(artifact)) if installed_verified => Err(ArtifactWriteError {
            error: Box::new(error),
            state: ArtifactInstallState::Installed(artifact),
        }),
        (Some(error), _) if renamed || uncertain => Err(ArtifactWriteError {
            error: Box::new(error),
            state: ArtifactInstallState::Uncertain,
        }),
        (Some(error), _) => Err(ArtifactWriteError {
            error: Box::new(error),
            state: ArtifactInstallState::NotInstalled,
        }),
    }
}

/// Result of a managed-agent operation, including durable delivery outcomes.
pub type Result<T> = std::result::Result<T, AgentError>;

fn fail(message: impl Into<String>) -> AgentError {
    AgentError::Delivery(message.into())
}

fn message_id(value: &str) -> bool {
    value.len() <= 255
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn name(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 32
        || value == "archive"
        || !value.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(fail(
            "agent name must start with a lowercase letter and contain 1-32 lowercase letters, digits or hyphens; 'archive' is reserved",
        ));
    }
    Ok(value)
}

/// The agent-name pattern of the shared on-disk formats: `[a-z][a-z0-9-]{0,31}`.
fn name_pattern(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A rename journal id: 32 lowercase hexadecimal digits.
fn journal_id_pattern(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Quote an optional simple string as `'value'` or `None`, so messages read alike in both editions.
fn repr(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| format!("'{value}'"))
}

/// One earlier name of a renamed agent, oldest first in [`AgentRecord::name_history`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NameHistoryEntry {
    name: String,
    renamed_at: f64,
    journal_id: String,
}

/// Validate a record's earlier names: each a valid name with a time and a unique journal id.
/// Earlier names one record keeps; rename refuses rather than exceed it.
const MAX_NAME_HISTORY: usize = 256;
/// Names evicted from the history that a record still remembers for liveness lookups.
const MAX_FORMER_NAMES: usize = 4096;
/// The harness pinning rule: the harness must be the pane's only foreground process. Anchors
/// written under an earlier rule (none, or a launcher-capable one) do not count.
const ANCHOR_RULE: u32 = 2;

fn valid_name_history(history: &[NameHistoryEntry]) -> bool {
    let mut journals = BTreeSet::new();
    history.len() <= MAX_NAME_HISTORY
        && history.iter().all(|entry| {
            name_pattern(&entry.name)
                && entry.renamed_at.is_finite()
                && journal_id_pattern(&entry.journal_id)
                && journals.insert(entry.journal_id.as_str())
        })
}

/// Schema tag of a rename journal; both editions read and finish each other's journals.
const RENAME_JOURNAL_SCHEMA: &str = "agentctl-rename/v1";
const RENAME_JOURNAL_KEYS: [&str; 11] = [
    "schema",
    "token",
    "old",
    "new",
    "adapter",
    "pane_id",
    "tab_id",
    "terminal_id",
    "workspace_id",
    "journal_id",
    "started_at",
];

/// `.renames/<token>.json`: reserves both names until a rename is complete.
#[derive(Clone, Debug, PartialEq)]
struct RenameJournal {
    token: String,
    old: String,
    new: String,
    adapter: String,
    pane_id: String,
    tab_id: String,
    terminal_id: Option<String>,
    workspace_id: Option<String>,
    journal_id: String,
    started_at: f64,
}

impl RenameJournal {
    /// Validate one rename journal written by either edition.
    fn parse(value: &Value, path: &Path) -> Result<Self> {
        let invalid = || fail(format!("invalid rename journal: {}", path.display()));
        let object = value.as_object().ok_or_else(invalid)?;
        if object.len() != RENAME_JOURNAL_KEYS.len()
            || RENAME_JOURNAL_KEYS
                .iter()
                .any(|key| !object.contains_key(*key))
        {
            return Err(invalid());
        }
        let text = |key: &str| object[key].as_str();
        let optional = |key: &str| match &object[key] {
            Value::Null => Ok(None),
            Value::String(value) => Ok(Some(value.clone())),
            _ => Err(invalid()),
        };
        let token = text("token").ok_or_else(invalid)?;
        let old = text("old").ok_or_else(invalid)?;
        let new = text("new").ok_or_else(invalid)?;
        let adapter = text("adapter").ok_or_else(invalid)?;
        let pane_id = text("pane_id").ok_or_else(invalid)?;
        let tab_id = text("tab_id").ok_or_else(invalid)?;
        let journal_id = text("journal_id").ok_or_else(invalid)?;
        let started_at = object["started_at"]
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(invalid)?;
        if text("schema") != Some(RENAME_JOURNAL_SCHEMA)
            || path.file_name().and_then(|name| name.to_str()) != Some(&format!("{token}.json"))
            || token.is_empty()
            || token.len() > 80
            || !token
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || !name_pattern(old)
            || !name_pattern(new)
            || old == new
            || !matches!(adapter, "herdr" | "herdr-foreign")
            || pane_id.is_empty()
            || tab_id.is_empty()
            || !journal_id_pattern(journal_id)
        {
            return Err(invalid());
        }
        Ok(Self {
            token: token.to_owned(),
            old: old.to_owned(),
            new: new.to_owned(),
            adapter: adapter.to_owned(),
            pane_id: pane_id.to_owned(),
            tab_id: tab_id.to_owned(),
            terminal_id: optional("terminal_id")?,
            workspace_id: optional("workspace_id")?,
            journal_id: journal_id.to_owned(),
            started_at,
        })
    }

    fn document(&self) -> Value {
        json!({
            "schema": RENAME_JOURNAL_SCHEMA,
            "token": self.token,
            "old": self.old,
            "new": self.new,
            "adapter": self.adapter,
            "pane_id": self.pane_id,
            "tab_id": self.tab_id,
            "terminal_id": self.terminal_id,
            "workspace_id": self.workspace_id,
            "journal_id": self.journal_id,
            "started_at": self.started_at,
        })
    }

    fn names(&self, candidates: &[&str]) -> bool {
        candidates.contains(&self.old.as_str()) || candidates.contains(&self.new.as_str())
    }
}

/// A fresh random rename journal id (32 lowercase hexadecimal digits, like a UUID4 hex).
fn new_journal_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: the pointer and length describe the unfilled tail of a live stack buffer.
        let read = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(fail(format!("cannot draw a rename journal id: {error}")));
        }
        filled += read as usize;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A Claude conversation id is a hyphenated UUID4, assigned before launch.
fn new_conversation_id() -> Result<String> {
    let hex = new_journal_id()?;
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..],
    ))
}

fn valid_metadata_text(value: &str) -> bool {
    !value.is_empty() && !value.contains('\0')
}

fn valid_environment_name(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn environment_names(environment: &[String]) -> Vec<String> {
    environment
        .iter()
        .filter_map(|entry| entry.split_once('=').map(|(name, _)| name.to_owned()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn claude_conversation_selector(argument: &str) -> bool {
    let key = argument.split_once('=').map_or(argument, |(key, _)| key);
    matches!(
        key,
        "--session-id" | "--resume" | "--continue" | "--fork-session" | "-r" | "-c"
    ) || (!argument.starts_with("--") && (argument.starts_with("-r") || argument.starts_with("-c")))
}

#[cfg(test)]
thread_local! {
    /// Makes the next rename fail after publishing the renamed record inside the old directory
    /// and before moving the directory, as a crash there would.
    static FAIL_RENAME_DIRECTORY_MOVE: Cell<bool> = const { Cell::new(false) };
}

/// Harness arguments for Codex/Claude/Muse presets, leaving permission policy untouched.
pub fn harness_arguments(
    harness: &str,
    model: Option<&str>,
    resume: Option<&str>,
    extra: &[String],
) -> Result<Vec<String>> {
    if harness.is_empty()
        || harness.len() > 64
        || !harness.as_bytes()[0].is_ascii_lowercase()
        || !harness
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(fail("harness must be a Herdr agent kind"));
    }
    if extra
        .iter()
        .any(|value| value.is_empty() || value.contains('\0'))
    {
        return Err(fail(
            "harness arguments must be nonempty and contain no NUL",
        ));
    }
    let mut arguments = Vec::new();
    match harness {
        "codex" => {
            if let Some(resume) = resume.filter(|value| !value.is_empty()) {
                arguments.extend(["resume".to_owned(), resume.to_owned()]);
            }
            arguments.push("--no-alt-screen".to_owned());
            if let Some(model) = model.filter(|value| !value.is_empty()) {
                arguments.extend(["--model".to_owned(), model.to_owned()]);
            }
        }
        "claude" => {
            if let Some(resume) = resume.filter(|value| !value.is_empty()) {
                arguments.extend(["--resume".to_owned(), resume.to_owned()]);
            }
            if let Some(model) = model.filter(|value| !value.is_empty()) {
                arguments.extend(["--model".to_owned(), model.to_owned()]);
            }
        }
        "muse" => {
            if let Some(model) = model.filter(|value| !value.is_empty()) {
                arguments.extend(["--model".to_owned(), model.to_owned()]);
            }
        }
        _ if model.is_some() || resume.is_some() => return Err(fail(
            "model and resume presets support codex/claude/muse; use harness arguments for other kinds",
        )),
        _ => {}
    }
    if arguments.iter().any(|value| value.contains('\0')) {
        return Err(fail("harness arguments must contain no NUL"));
    }
    arguments.extend_from_slice(extra);
    if harness == "muse" {
        if let Some(resume) = resume.filter(|value| !value.is_empty()) {
            arguments.extend(["resume".to_owned(), resume.to_owned()]);
        }
    }
    Ok(arguments)
}

/// Validate literal `KEY=VALUE` entries for a newly created terminal.
pub fn environment_entries(values: &[String]) -> Result<Vec<String>> {
    let mut entries = Vec::with_capacity(values.len());
    for entry in values {
        let Some((name, value)) = entry.split_once('=') else {
            return Err(fail("environment entry must use KEY=VALUE"));
        };
        let valid_name = name
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if name.contains('\0') || !valid_name {
            return Err(fail(
                "environment variable name must match [A-Za-z_][A-Za-z0-9_]*",
            ));
        }
        if value.contains('\0') {
            return Err(fail("environment variable value must contain no NUL"));
        }
        entries.push(entry.clone());
    }
    Ok(entries)
}

/// Lifecycle operations in addition to the existing interactive messaging API.
pub trait ManagedApi: AgentApi {
    /// Read kernel evidence for one pinned process generation; unavailable evidence is unknown.
    fn process_liveness(&self, _expected: &CustomProcessIdentity) -> ProcessLiveness {
        ProcessLiveness::Unknown
    }
    /// Resolve a unique workspace label.
    fn workspace_id_for_label(&self, label: &str) -> crate::error::Result<Option<String>>;
    /// Create a workspace and return its workspace, tab, and pane IDs.
    fn create_workspace(
        &self,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> crate::error::Result<(String, String, String)>;
    /// Create a fresh labelled tab without stealing focus.
    fn create_tab(
        &self,
        workspace: &str,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> crate::error::Result<String>;
    /// Capture both ownership IDs in the same tab-allocation response.
    fn create_tab_with_pane(
        &self,
        workspace: &str,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> crate::error::Result<(String, String)>;
    /// Move one exact pane into a fresh labelled tab in another workspace.
    fn move_pane_to_new_tab(
        &self,
        _pane: &str,
        _workspace: &str,
        _label: &str,
    ) -> crate::error::Result<Pane> {
        Err(crate::error::AdapterError::unavailable(
            "cross-workspace pane moves are unavailable",
        ))
    }
    /// Close exactly one owned pane, preserving any concurrently added siblings.
    fn close_pane(&self, pane: &str) -> crate::error::Result<()>;
    /// Focus an existing pane for direct human interaction.
    fn focus_pane(&self, pane: &str) -> crate::error::Result<()>;
    /// Rename one owned tab.
    fn rename_tab(&self, tab: &str, label: &str) -> crate::error::Result<()>;
    /// Start an interactive harness in a fresh shell pane.
    fn start_agent(
        &self,
        name: &str,
        harness: &str,
        pane: &str,
        args: &[String],
        timeout: Duration,
    ) -> crate::error::Result<()>;
    /// The fixed install location of a harness executable.
    fn harness_executable(&self, kind: &str) -> crate::error::Result<PathBuf> {
        crate::client::resolve_harness_executable(kind)
    }
    /// Run a slot-boxed harness behind a root relay and pin the relayed program.
    fn start_relay_agent(
        &self,
        _kind: &str,
        _pane: &str,
        _command_line: &str,
        _timeout: Duration,
        _persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> crate::error::Result<()>,
    ) -> crate::error::Result<CustomProcessIdentity> {
        Err(crate::error::AdapterError::unavailable(
            "slot relay launch is unavailable",
        ))
    }
    /// The program running behind a root slot relay in one pane, if any.
    fn relay_process(&self, _pane: &str) -> crate::error::Result<Option<CustomProcessIdentity>> {
        Err(crate::error::AdapterError::unavailable(
            "slot relay inspection is unavailable",
        ))
    }
    /// Require the recorded program to still run behind this pane's root relay.
    fn verify_relay_harness(
        &self,
        pane: &str,
        expected: Option<&CustomProcessIdentity>,
    ) -> crate::error::Result<()> {
        let observed = self.relay_process(pane)?;
        if observed.is_none() || observed.as_ref() != expected {
            return Err(crate::error::AdapterError::unavailable(format!(
                "the recorded program is not running behind the slot relay in pane {pane}"
            )));
        }
        Ok(())
    }
    /// Herdr's live screen-rule verdict for one pane: (agent kind, state).
    fn explain_agent(&self, _pane: &str) -> crate::error::Result<(Option<String>, String)> {
        Err(crate::error::AdapterError::unavailable(
            "screen-rule explanation is unavailable",
        ))
    }
    /// Label one exact pane as a harness kind with a lifecycle state.
    fn report_pane_agent(
        &self,
        _pane: &str,
        _kind: &str,
        _state: &str,
    ) -> crate::error::Result<()> {
        Err(crate::error::AdapterError::unavailable(
            "pane agent reports are unavailable",
        ))
    }
    /// Replace a fresh pane's shell with a slot-boxed shell; return the pane's shell PID.
    fn enter_slot_sandbox(
        &self,
        _pane: &str,
        _command_line: &str,
        _timeout: Duration,
    ) -> crate::error::Result<u64> {
        Err(crate::error::AdapterError::unavailable(
            "slot sandbox entry is unavailable",
        ))
    }
    /// Start a custom harness through `pane run` and verify the foreground process.
    fn start_pane_agent(
        &self,
        _name: &str,
        _harness: &str,
        _pane: &str,
        _args: &[String],
        _timeout: Duration,
        _persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        Err(crate::error::AdapterError::unavailable(
            "custom pane harness launch is unavailable",
        ))
    }
    /// Run one literal executable in a fresh shell pane and pin it as the foreground process.
    fn start_pane_command(
        &self,
        _pane: &str,
        _executable: &Path,
        _arguments: &[String],
        _timeout: Duration,
    ) -> crate::error::Result<CustomProcessIdentity> {
        Err(crate::error::AdapterError::unavailable(
            "pane command launch is unavailable",
        ))
    }
    /// Report whether a pinned command process is still the pane's foreground process.
    fn pane_runs_command(
        &self,
        _pane: &str,
        _identity: &CustomProcessIdentity,
    ) -> crate::error::Result<bool> {
        Err(crate::error::AdapterError::unavailable(
            "pane command verification is unavailable",
        ))
    }
    /// Require the configured custom harness in one exact foreground pane.
    fn verify_custom_harness(
        &self,
        _pane: &str,
        _harness: &str,
        _identity: Option<&CustomProcessIdentity>,
    ) -> crate::error::Result<()> {
        Err(crate::error::AdapterError::unavailable(
            "custom pane harness verification is unavailable",
        ))
    }
    /// Prove the pane has returned to its original shell process group.
    fn pane_is_idle_shell(&self, _pane: &str) -> crate::error::Result<bool> {
        Err(crate::error::AdapterError::unavailable(
            "idle shell verification is unavailable",
        ))
    }
    /// Capture the exact process generation of the shell Herdr owns for a pane.
    fn pane_shell_identity(&self, _pane: &str) -> crate::error::Result<CustomProcessIdentity> {
        Err(crate::error::AdapterError::unavailable(
            "pane shell identity is unavailable",
        ))
    }
    /// Prove both idle-shell state and the exact process generation captured earlier.
    fn pane_is_same_idle_shell(
        &self,
        _pane: &str,
        _expected: &CustomProcessIdentity,
    ) -> crate::error::Result<bool> {
        Err(crate::error::AdapterError::unavailable(
            "identity-bound idle shell verification is unavailable",
        ))
    }
    /// Capture one exact supported idle-shell generation with no descendants.
    fn pane_idle_shell_identity(
        &self,
        _pane: &str,
    ) -> crate::error::Result<Option<PaneShellProof>> {
        Err(crate::error::AdapterError::unavailable(
            "idle shell identity proof is unavailable",
        ))
    }
    /// Cancellation-aware custom harness verification.
    fn verify_custom_harness_with_runtime(
        &self,
        pane: &str,
        harness: &str,
        identity: Option<&CustomProcessIdentity>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        if runtime.cancelled() {
            return Err(crate::error::AdapterError::unavailable(
                "Herdr control operation was cancelled",
            ));
        }
        self.verify_custom_harness(pane, harness, identity)
    }
    /// Insert literal text without a submission key.
    fn send_text(&self, _pane: &str, _text: &str) -> crate::error::Result<()> {
        Err(crate::error::AdapterError::unavailable(
            "literal pane text insertion is unavailable",
        ))
    }
    /// Cancellation-aware literal text insertion.
    fn send_text_with_runtime(
        &self,
        pane: &str,
        text: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        if runtime.cancelled() {
            return Err(crate::error::AdapterError::unavailable(
                "Herdr control operation was cancelled",
            ));
        }
        self.send_text(pane, text)
    }
    /// Recent scrollback without wrapping, to find text a prompt left in a pane.
    fn read_scrollback(&self, pane: &str) -> crate::error::Result<String> {
        self.read(pane, "recent-unwrapped", Some(SCROLLBACK_LINES))
    }
    /// Resolve an exact live Herdr agent name.
    fn agent_pane(&self, name: &str) -> crate::error::Result<String>;
    /// Resolve an exact live Herdr agent name with service cancellation.
    fn agent_pane_with_runtime(
        &self,
        name: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<String> {
        if runtime.cancelled() {
            return Err(crate::error::AdapterError::unavailable(
                "Herdr control operation was cancelled",
            ));
        }
        self.agent_pane(name)
    }
    /// Report the explicitly supplied native session identity.
    fn report_agent_session(
        &self,
        name: &str,
        pane: &str,
        kind: &str,
        session: &str,
    ) -> crate::error::Result<()>;
    /// Send one explicit key to the named pane.
    fn send_keys(&self, pane: &str, key: &str) -> crate::error::Result<()>;
    /// Send one key with service cancellation.
    fn send_keys_with_runtime(
        &self,
        pane: &str,
        key: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        if runtime.cancelled() {
            return Err(crate::error::AdapterError::unavailable(
                "Herdr control operation was cancelled",
            ));
        }
        self.send_keys(pane, key)
    }
    /// Close one owned tab; never close its workspace.
    fn close_tab(&self, tab: &str) -> crate::error::Result<()>;
    /// Resolve an exact live Herdr agent name to its pane, tab and terminal.
    fn agent_identity(&self, _name: &str) -> crate::error::Result<AgentIdentity> {
        Err(AdapterError::unavailable(
            "Herdr agent identity lookup is unavailable",
        ))
    }
    /// Give the agent in one exact pane a new Herdr agent name.
    fn rename_agent(&self, _pane: &str, _name: &str) -> crate::error::Result<()> {
        Err(AdapterError::unavailable(
            "Herdr agent rename is unavailable",
        ))
    }
    /// Every named Herdr agent, mapped to its pane.
    fn agent_names(&self) -> crate::error::Result<BTreeMap<String, String>> {
        Err(AdapterError::unavailable(
            "Herdr agent listing is unavailable",
        ))
    }
    /// Return the label of one exact tab.
    fn tab_label(&self, _tab: &str) -> crate::error::Result<String> {
        Err(AdapterError::unavailable("Herdr tab lookup is unavailable"))
    }
    /// Every tab of one workspace, mapped to its label.
    fn tab_labels(&self, _workspace: &str) -> crate::error::Result<BTreeMap<String, String>> {
        Err(AdapterError::unavailable(
            "Herdr tab listing is unavailable",
        ))
    }
    /// Pin the pane's foreground process-group leader; `None` when it cannot be pinned.
    fn harness_identity(
        &self,
        _pane: &str,
        _kind: &str,
    ) -> crate::error::Result<Option<CustomProcessIdentity>> {
        Ok(None)
    }
    /// Is the pinned harness process still the pane's foreground process-group leader?
    fn verify_harness_identity(
        &self,
        _pane: &str,
        _expected: &CustomProcessIdentity,
    ) -> crate::error::Result<bool> {
        Err(AdapterError::unavailable(
            "harness process verification is unavailable",
        ))
    }
    /// Does the running server refuse input whose expected terminal does not match?
    fn input_expect_supported(&self) -> crate::error::Result<bool> {
        Ok(false)
    }
    /// Return visible rows with SGR styling retained, for composer inspection.
    fn read_screen_with_runtime(
        &self,
        pane: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<String> {
        self.read_with_runtime(
            pane,
            "visible",
            Some(crate::submission::SCREEN_LINES),
            runtime,
        )
    }
    /// Insert literal text, refused unless the pane holds `expect_terminal` when given.
    fn send_text_expect(
        &self,
        pane: &str,
        text: &str,
        expect_terminal: Option<&str>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        match expect_terminal {
            None => self.send_text_with_runtime(pane, text, runtime),
            Some(_) => Err(AdapterError::unavailable(
                "terminal-checked input is unavailable",
            )),
        }
    }
    /// Send one key, refused unless the pane holds `expect_terminal` when given.
    fn send_keys_expect(
        &self,
        pane: &str,
        key: &str,
        expect_terminal: Option<&str>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        match expect_terminal {
            None => self.send_keys_with_runtime(pane, key, runtime),
            Some(_) => Err(AdapterError::unavailable(
                "terminal-checked input is unavailable",
            )),
        }
    }
    /// Submit text plus Enter natively, refused unless the pane holds `expect_terminal`.
    fn agent_prompt_expect(
        &self,
        pane: &str,
        text: &str,
        expect_terminal: Option<&str>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        match expect_terminal {
            None => self.run_with_runtime(pane, text, runtime),
            Some(_) => Err(AdapterError::unavailable(
                "terminal-checked input is unavailable",
            )),
        }
    }
    /// Submit one prompt whose every input effect runs through `terminal`.
    ///
    /// The default uses the native prompt primitive and proves nothing.
    fn prompt_guarded(
        &self,
        pane: &str,
        text: &str,
        terminal: &dyn GuardedInput,
        _runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Submission> {
        terminal
            .native_prompt(pane, text)
            .map(|()| Submission::Unconfirmed)
    }
    /// Submit one prompt to a known composer harness through `terminal`'s guarded effects.
    fn submit_guarded(
        &self,
        pane: &str,
        _harness: &str,
        text: &str,
        terminal: &dyn GuardedInput,
        _runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Submission> {
        terminal
            .native_prompt(pane, text)
            .map(|()| Submission::Unconfirmed)
    }
}

impl ManagedApi for HerdrClient {
    fn process_liveness(&self, expected: &CustomProcessIdentity) -> ProcessLiveness {
        HerdrClient::process_liveness(self, expected)
    }
    fn agent_pane(&self, name: &str) -> crate::error::Result<String> {
        HerdrClient::agent_pane(self, name)
    }
    fn agent_pane_with_runtime(
        &self,
        name: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<String> {
        HerdrClient::agent_pane_with_cancellation(self, name, &|| runtime.cancelled())
    }
    fn report_agent_session(
        &self,
        name: &str,
        pane: &str,
        kind: &str,
        session: &str,
    ) -> crate::error::Result<()> {
        HerdrClient::report_agent_session(self, name, pane, kind, session)
    }
    fn send_keys(&self, pane: &str, key: &str) -> crate::error::Result<()> {
        HerdrClient::send_keys(self, pane, key)
    }
    fn send_keys_with_runtime(
        &self,
        pane: &str,
        key: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::send_keys_with_cancellation(self, pane, key, &|| runtime.cancelled())
    }

    fn workspace_id_for_label(&self, label: &str) -> crate::error::Result<Option<String>> {
        HerdrClient::workspace_id_for_label(self, label)
    }
    fn create_workspace(
        &self,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> crate::error::Result<(String, String, String)> {
        HerdrClient::create_workspace(self, label, cwd, environment)
    }
    fn create_tab(
        &self,
        workspace: &str,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> crate::error::Result<String> {
        HerdrClient::create_tab(self, workspace, label, cwd, environment)
    }
    fn create_tab_with_pane(
        &self,
        workspace: &str,
        label: &str,
        cwd: &str,
        environment: &[String],
    ) -> crate::error::Result<(String, String)> {
        HerdrClient::create_tab_with_pane(self, workspace, label, cwd, environment)
    }
    fn move_pane_to_new_tab(
        &self,
        pane: &str,
        workspace: &str,
        label: &str,
    ) -> crate::error::Result<Pane> {
        HerdrClient::move_pane_to_new_tab(self, pane, workspace, label)
    }
    fn close_pane(&self, pane: &str) -> crate::error::Result<()> {
        HerdrClient::close_pane(self, pane)
    }
    fn focus_pane(&self, pane: &str) -> crate::error::Result<()> {
        HerdrClient::focus_pane(self, pane)
    }
    fn rename_tab(&self, tab: &str, label: &str) -> crate::error::Result<()> {
        HerdrClient::rename_tab(self, tab, label)
    }
    fn start_agent(
        &self,
        name: &str,
        harness: &str,
        pane: &str,
        args: &[String],
        timeout: Duration,
    ) -> crate::error::Result<()> {
        HerdrClient::start_agent(self, name, harness, pane, args, timeout)
    }
    fn enter_slot_sandbox(
        &self,
        pane: &str,
        command_line: &str,
        timeout: Duration,
    ) -> crate::error::Result<u64> {
        HerdrClient::enter_slot_sandbox(self, pane, command_line, timeout)
    }
    fn start_relay_agent(
        &self,
        kind: &str,
        pane: &str,
        command_line: &str,
        timeout: Duration,
        persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> crate::error::Result<()>,
    ) -> crate::error::Result<CustomProcessIdentity> {
        HerdrClient::start_relay_agent(self, kind, pane, command_line, timeout, persist_identity)
    }
    fn relay_process(&self, pane: &str) -> crate::error::Result<Option<CustomProcessIdentity>> {
        HerdrClient::relay_process(self, pane)
    }
    fn explain_agent(&self, pane: &str) -> crate::error::Result<(Option<String>, String)> {
        HerdrClient::explain_agent(self, pane)
    }
    fn report_pane_agent(&self, pane: &str, kind: &str, state: &str) -> crate::error::Result<()> {
        HerdrClient::report_pane_agent(self, pane, kind, state)
    }
    fn start_pane_agent(
        &self,
        name: &str,
        harness: &str,
        pane: &str,
        args: &[String],
        timeout: Duration,
        persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        HerdrClient::start_pane_agent(self, name, harness, pane, args, timeout, persist_identity)
    }
    fn start_pane_command(
        &self,
        pane: &str,
        executable: &Path,
        arguments: &[String],
        timeout: Duration,
    ) -> crate::error::Result<CustomProcessIdentity> {
        HerdrClient::start_pane_command(self, pane, executable, arguments, timeout)
    }
    fn pane_runs_command(
        &self,
        pane: &str,
        identity: &CustomProcessIdentity,
    ) -> crate::error::Result<bool> {
        HerdrClient::pane_runs_command(self, pane, identity)
    }
    fn verify_custom_harness(
        &self,
        pane: &str,
        harness: &str,
        identity: Option<&CustomProcessIdentity>,
    ) -> crate::error::Result<()> {
        HerdrClient::verify_custom_harness(self, pane, harness, identity)
    }
    fn pane_is_idle_shell(&self, pane: &str) -> crate::error::Result<bool> {
        HerdrClient::pane_is_idle_shell(self, pane)
    }
    fn pane_shell_identity(&self, pane: &str) -> crate::error::Result<CustomProcessIdentity> {
        HerdrClient::pane_shell_identity(self, pane)
    }
    fn pane_is_same_idle_shell(
        &self,
        pane: &str,
        expected: &CustomProcessIdentity,
    ) -> crate::error::Result<bool> {
        HerdrClient::pane_is_same_idle_shell(self, pane, expected)
    }
    fn pane_idle_shell_identity(&self, pane: &str) -> crate::error::Result<Option<PaneShellProof>> {
        HerdrClient::pane_idle_shell_identity(self, pane)
    }
    fn verify_custom_harness_with_runtime(
        &self,
        pane: &str,
        harness: &str,
        identity: Option<&CustomProcessIdentity>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::verify_custom_harness_with_cancellation(self, pane, harness, identity, &|| {
            runtime.cancelled()
        })
    }
    fn send_text(&self, pane: &str, text: &str) -> crate::error::Result<()> {
        HerdrClient::send_text(self, pane, text)
    }
    fn send_text_with_runtime(
        &self,
        pane: &str,
        text: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::send_text_with_cancellation(self, pane, text, &|| runtime.cancelled())
    }
    fn close_tab(&self, tab: &str) -> crate::error::Result<()> {
        HerdrClient::close_tab(self, tab)
    }
    fn agent_identity(&self, name: &str) -> crate::error::Result<AgentIdentity> {
        HerdrClient::agent_identity(self, name)
    }
    fn rename_agent(&self, pane: &str, name: &str) -> crate::error::Result<()> {
        HerdrClient::rename_agent(self, pane, name)
    }
    fn agent_names(&self) -> crate::error::Result<BTreeMap<String, String>> {
        HerdrClient::agent_names(self)
    }
    fn tab_label(&self, tab: &str) -> crate::error::Result<String> {
        HerdrClient::tab_label(self, tab)
    }
    fn tab_labels(&self, workspace: &str) -> crate::error::Result<BTreeMap<String, String>> {
        HerdrClient::tab_labels(self, workspace)
    }
    fn harness_identity(
        &self,
        pane: &str,
        kind: &str,
    ) -> crate::error::Result<Option<CustomProcessIdentity>> {
        HerdrClient::harness_identity(self, pane, kind)
    }
    fn verify_harness_identity(
        &self,
        pane: &str,
        expected: &CustomProcessIdentity,
    ) -> crate::error::Result<bool> {
        HerdrClient::verify_harness_identity(self, pane, expected)
    }
    fn input_expect_supported(&self) -> crate::error::Result<bool> {
        HerdrClient::input_expect_supported(self)
    }
    fn read_screen_with_runtime(
        &self,
        pane: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<String> {
        HerdrClient::read_screen_with_cancellation(self, pane, &|| runtime.cancelled())
    }
    fn send_text_expect(
        &self,
        pane: &str,
        text: &str,
        expect_terminal: Option<&str>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::send_text_expecting(self, pane, text, expect_terminal, &|| runtime.cancelled())
    }
    fn send_keys_expect(
        &self,
        pane: &str,
        key: &str,
        expect_terminal: Option<&str>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::send_keys_expecting(self, pane, key, expect_terminal, &|| runtime.cancelled())
    }
    fn agent_prompt_expect(
        &self,
        pane: &str,
        text: &str,
        expect_terminal: Option<&str>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        HerdrClient::agent_prompt(self, pane, text, expect_terminal, &|| runtime.cancelled())
    }
    fn prompt_guarded(
        &self,
        pane: &str,
        text: &str,
        terminal: &dyn GuardedInput,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Submission> {
        let harness =
            match HerdrClient::pane_info_with_cancellation(self, pane, &|| runtime.cancelled()) {
                Ok(info) => info.agent.unwrap_or_default(),
                Err(error) => {
                    return Ok(Submission::NotStaged(format!(
                        "pane {pane}: harness lookup failed before typing: {error}"
                    )))
                }
            };
        if crate::submission::verifies(&harness, text) {
            return crate::submission::submit_verified(
                &GuardedPrompt(terminal),
                pane,
                &harness,
                text,
                SubmitTimeouts::default(),
                runtime,
            );
        }
        terminal
            .native_prompt(pane, text)
            .map(|()| Submission::Unconfirmed)
    }
    fn submit_guarded(
        &self,
        pane: &str,
        harness: &str,
        text: &str,
        terminal: &dyn GuardedInput,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Submission> {
        crate::submission::submit_verified(
            &GuardedPrompt(terminal),
            pane,
            harness,
            text,
            SubmitTimeouts::default(),
            runtime,
        )
    }
}

/// Recovery provenance, separate from the observed live-routing identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeSession {
    schema: String,
    agent: String,
    value: String,
    source: String,
}

impl NativeSession {
    fn new(agent: &str, value: &str, source: &str) -> Self {
        Self {
            schema: "agentctl-native-session/v1".to_owned(),
            agent: agent.to_owned(),
            value: value.to_owned(),
            source: source.to_owned(),
        }
    }

    fn valid_for(&self, record: &AgentRecord) -> bool {
        self.schema == "agentctl-native-session/v1"
            && self.agent == record.harness
            && valid_metadata_text(&self.value)
            && matches!(self.source.as_str(), "observed" | "asserted")
            && record
                .session_agent
                .as_deref()
                .is_none_or(|agent| agent == self.agent)
            && record
                .session_value
                .as_deref()
                .is_none_or(|value| value == self.value)
            && record
                .resume
                .as_deref()
                .is_none_or(|value| value == self.value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AgentRecord {
    /// Private launch storage; serialized records cannot request another write directory.
    #[serde(skip)]
    storage_directory: Option<StorageDirectory>,
    /// The nested v2/v3 row this record was decoded from and is written back as.
    #[serde(skip)]
    nested: Option<nested::NestedRecord>,
    #[serde(default = "herdr_adapter")]
    adapter: String,
    #[serde(default = "interactive_mode")]
    mode: String,
    #[serde(default = "herdr_adapter")]
    backend: String,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    runtime_home: Option<String>,
    #[serde(default)]
    pane_reported_by_agentctl: bool,
    #[serde(default)]
    custom_process_identity: Option<CustomProcessIdentity>,
    #[serde(default)]
    foreign_shell_identity: Option<CustomProcessIdentity>,
    /// Herdr terminal the pane held when agentctl anchored this record.
    #[serde(default)]
    terminal_id: Option<String>,
    /// Kernel identity of the harness process anchored at start, adopt or anchor.
    #[serde(default)]
    harness_identity: Option<CustomProcessIdentity>,
    /// Earlier names, oldest first.
    #[serde(default)]
    name_history: Vec<NameHistoryEntry>,
    /// Which pinning rule produced `harness_identity`; only [`ANCHOR_RULE`] anchors count.
    #[serde(default)]
    anchor_rule: Option<u32>,
    /// Earlier names evicted from the capped `name_history`, kept for liveness lookups.
    #[serde(default)]
    former_names: Vec<String>,
    /// Native conversation to recover, which is never an asserted routing anchor.
    #[serde(default)]
    native_session: Option<NativeSession>,
    /// Owner-configured launch profile, when selected.
    #[serde(default)]
    profile: Option<String>,
    /// Slot boxing policy, when this launch used a wrkslots slot.
    #[serde(default)]
    slot: Option<String>,
    #[serde(default)]
    slot_project: Option<String>,
    #[serde(default)]
    slot_isolation: Option<String>,
    /// Requested structured effort, separate from a harness's effective fallback.
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Names only; literal environment values are not recovery metadata.
    #[serde(default)]
    environment_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agentcloud: Option<cloud::CloudRecord>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
    name: String,
    token: String,
    harness: String,
    cwd: String,
    created_at: f64,
    schema: u32,
    lifecycle: String,
    workspace_id: Option<String>,
    tab_id: Option<String>,
    pane_id: Option<String>,
    session_agent: Option<String>,
    session_value: Option<String>,
    model: Option<String>,
    resume: Option<String>,
    arguments: Vec<String>,
    #[serde(default)]
    startup_warning: Option<String>,
    #[serde(default)]
    effective_reasoning_effort: Option<String>,
    error: Option<String>,
    goal: Option<String>,
    goal_delivery: Option<String>,
    goal_session_id: Option<String>,
    goal_command: Option<Vec<String>>,
    #[serde(default)]
    goal_messages: BTreeMap<String, String>,
    goal_message_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StorageDirectory {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeadPaneProof {
    info: AgentPaneInfo,
    presentation: Pane,
    shell: PaneShellProof,
}

#[derive(Clone, Debug)]
struct LegacyRecordSnapshot {
    record: AgentRecord,
    content: Vec<u8>,
    digest: String,
    directory_device: u64,
    directory_inode: u64,
}

#[derive(Clone, Debug)]
struct ManagedRecordSnapshot {
    record: AgentRecord,
    content: Vec<u8>,
    directory_device: u64,
    directory_inode: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct DeadAdoptionSnapshot {
    record: AgentRecord,
    content: Vec<u8>,
    record_identity: (u64, u64),
    directory_identity: (u64, u64),
}

#[derive(Debug)]
struct RetirementQueue {
    directory: Option<PinnedParentDirectory>,
    locks: Vec<(&'static str, File)>,
}

impl ManagedRecordSnapshot {
    fn same_generation(&self, other: &Self) -> bool {
        self.record == other.record
            && self.content == other.content
            && self.directory_device == other.directory_device
            && self.directory_inode == other.directory_inode
    }
}

#[derive(Debug)]
struct PinnedAgentDirectory {
    name: String,
    path: PathBuf,
    file: File,
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct PinnedParentDirectory {
    path: PathBuf,
    file: File,
    device: u64,
    inode: u64,
}

impl LegacyRecordSnapshot {
    fn same_generation(&self, other: &Self) -> bool {
        self.content == other.content
            && self.digest == other.digest
            && self.directory_device == other.directory_device
            && self.directory_inode == other.directory_inode
    }
}

fn herdr_adapter() -> String {
    "herdr".to_owned()
}
fn interactive_mode() -> String {
    "interactive".to_owned()
}

impl AgentRecord {
    /// The pinned harness process, if the current pinning rule produced it.
    fn harness_anchor(&self) -> Option<&CustomProcessIdentity> {
        if self.anchor_rule == Some(ANCHOR_RULE) {
            self.harness_identity.as_ref()
        } else {
            None
        }
    }

    /// Capture only the native identity the launched pane reports, and reject a different
    /// conversation before any startup brief can be delivered.
    fn capture_native_session(&mut self, info: &AgentPaneInfo) -> Result<()> {
        let (agent, value) = match (&info.session_agent, &info.session_value) {
            (None, None) => return Ok(()),
            (Some(agent), Some(value)) if agent == &self.harness && valid_metadata_text(value) => {
                (agent, value)
            }
            _ => {
                return Err(fail(
                    "started agent reports an invalid or different native session provider",
                ))
            }
        };
        if let Some(native) = self.native_session.as_ref() {
            if &native.agent != agent || &native.value != value {
                return Err(fail(
                    "started agent reports a different native conversation than requested",
                ));
            }
        } else {
            self.native_session = Some(NativeSession::new(agent, value, "observed"));
        }
        Ok(())
    }

    fn validate_loaded(&self, path: &Path, agent_name: &str) -> Result<()> {
        if self.name != agent_name
            || self.schema != 1
            || self.token.is_empty()
            || self.token.len() > 80
            || !self
                .token
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || self.harness.is_empty()
            || self.cwd.is_empty()
            || self.lifecycle.is_empty()
            || !self.created_at.is_finite()
            || self
                .goal_message_id
                .as_deref()
                .is_some_and(|value| !message_id(value))
            || self
                .goal_messages
                .iter()
                .any(|(key, value)| !message_id(key) || value.is_empty())
            || self.goal_command.as_ref().is_some_and(|command| {
                command.is_empty()
                    || command
                        .iter()
                        .any(|value| value.is_empty() || value.contains('\0'))
            })
            || self.startup_warning.as_deref().is_some_and(|warning| {
                warning.len() > 256 || !warning.is_ascii() || warning.contains(['\r', '\n', '\0'])
            })
            || self
                .effective_reasoning_effort
                .as_deref()
                .is_some_and(|effort| {
                    !matches!(
                        effort,
                        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                    )
                })
            || self
                .custom_process_identity
                .as_ref()
                .is_some_and(|identity| {
                    !matches!(
                        (self.adapter.as_str(), self.harness.as_str()),
                        ("herdr-pane", "muse") | ("herdr-relay", "claude" | "codex")
                    ) || self.pane_id.as_deref().is_none_or(str::is_empty)
                        || !identity.valid()
                })
            || self
                .foreign_shell_identity
                .as_ref()
                .is_some_and(|identity| {
                    self.adapter != "herdr-foreign"
                        || self.pane_id.as_deref().is_none_or(str::is_empty)
                        || !identity.valid()
                })
            || self.is_cloud()
                != (self.agentcloud.is_some() && self.harness == cloud::CLOUD_HARNESS)
            || self.agentcloud.as_ref().is_some_and(|record| {
                !record.valid()
                    || self
                        .session_value
                        .as_deref()
                        .is_some_and(|session| !cloud::valid_session_id(session))
            })
        {
            return Err(fail(format!("invalid agent record: {}", path.display())));
        }
        if self.terminal_id.as_deref().is_some_and(|terminal| {
            terminal.is_empty() || terminal.len() > 128 || !terminal.is_ascii()
        }) {
            return Err(fail(format!("invalid terminal id in {}", path.display())));
        }
        if self
            .harness_identity
            .as_ref()
            .is_some_and(|identity| !identity.valid())
        {
            return Err(fail(format!(
                "invalid harness identity in {}",
                path.display()
            )));
        }
        if self.former_names.len() > MAX_FORMER_NAMES
            || self.former_names.iter().any(|former| !name_pattern(former))
            || self.former_names.iter().collect::<BTreeSet<_>>().len() != self.former_names.len()
        {
            return Err(fail(format!("invalid former names in {}", path.display())));
        }
        if !valid_name_history(&self.name_history) {
            return Err(fail(format!("invalid name history in {}", path.display())));
        }
        if self
            .native_session
            .as_ref()
            .is_some_and(|native| !native.valid_for(self))
        {
            return Err(fail(format!(
                "invalid or conflicting native session recovery metadata in {}",
                path.display()
            )));
        }
        if self
            .profile
            .as_deref()
            .is_some_and(|value| !name_pattern(value))
            || self
                .slot
                .as_deref()
                .is_some_and(|value| !valid_metadata_text(value))
            || self
                .slot_project
                .as_deref()
                .is_some_and(|value| !valid_metadata_text(value) || !Path::new(value).is_absolute())
            || self
                .slot_isolation
                .as_deref()
                .is_some_and(|value| !SLOT_ISOLATIONS.contains(&value))
            || self.reasoning_effort.as_deref().is_some_and(|value| {
                !matches!(
                    value,
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                )
            })
            || self
                .environment_names
                .iter()
                .any(|value| !valid_environment_name(value))
            || self.environment_names.iter().collect::<BTreeSet<_>>().len()
                != self.environment_names.len()
        {
            return Err(fail(format!(
                "invalid launch recovery metadata in {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn supported(&self) -> Result<()> {
        if self.is_cloud() {
            return Err(fail(format!(
                "agent {:?} is an agentcloud session; this operation drives Herdr-hosted harnesses only. Use agentctl send, status, read, wait, attach, pause, resume, or stop; for a goal, run agentctl send {} '/goal OBJECTIVE'",
                self.name, self.name
            )));
        }
        if !matches!(
            self.adapter.as_str(),
            "herdr" | "herdr-pane" | "herdr-foreign" | "herdr-relay"
        ) || self.mode != "interactive"
            || self.backend != "herdr"
        {
            return Err(fail(turn_runner::unsupported_message(self)));
        }
        Ok(())
    }
    fn input_allowed(&self) -> Result<()> {
        self.supported()?;
        if self.paused {
            return Err(fail(format!("agent {:?} is paused for human input; run agentctl resume before sending automation input", self.name)));
        }
        Ok(())
    }

    fn target(&self) -> Result<Target> {
        self.supported()?;
        if self.pane_id.as_deref().is_none_or(str::is_empty) {
            return Err(fail(format!(
                "agent {:?} has no confirmed pane; inspect its launch error",
                self.name
            )));
        }
        let (session_agent, session_value) = self.routing_session();
        Ok(Target {
            pane_id: self.pane_id.clone(),
            session_agent,
            session_value,
            expected_agent: Some(self.harness.clone()),
            expected_cwd: Some(PathBuf::from(&self.cwd)),
            expected_workspace: None,
        })
    }
}

fn goal_replacement_selected(screen: &str, objective: &str) -> bool {
    let screen = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    let objective = objective.split_whitespace().collect::<Vec<_>>().join(" ");
    screen.contains("Replace goal?") && ["›", "❯"].iter().any(|marker| screen.contains(&format!("New objective: {objective} {marker} 1. Replace current goal Set the new objective and start it now")))
        && screen.contains("2. Cancel Keep the current goal") && screen.contains("Press enter to confirm or esc to go back")
}

#[derive(Clone, Debug)]
struct CustomPaneSubmission {
    staged_screen: String,
    text: String,
    prior_transcript_count: usize,
}

struct WorkspaceClient<'a, A: ManagedApi + ?Sized> {
    client: &'a A,
    record: &'a AgentRecord,
    goal_objective: Mutex<Option<String>>,
    custom_submission: Mutex<Option<CustomPaneSubmission>>,
    queue: Option<&'a Path>,
    check_prompt: bool,
    expected_workspace: Option<String>,
    /// A longer read-back limit than [`READBACK`], for a just-started harness.
    readback: Option<Duration>,
}

impl<A: ManagedApi + ?Sized> WorkspaceClient<'_, A> {
    fn uses_muse_composer(&self) -> bool {
        self.record.harness == "muse"
            && matches!(self.record.adapter.as_str(), "herdr-pane" | "herdr-foreign")
    }

    fn verify_muse_process(&self, pane_id: &str) -> crate::error::Result<()> {
        self.verify_muse_process_with_runtime(pane_id, &agent::SystemRuntime::default())
    }

    fn verify_muse_process_with_runtime(
        &self,
        pane_id: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        if self.record.adapter == "herdr-pane" {
            return self.client.verify_custom_harness_with_runtime(
                pane_id,
                &self.record.harness,
                self.record.custom_process_identity.as_ref(),
                runtime,
            );
        }
        if runtime.cancelled() {
            return Err(AdapterError::unavailable(
                "Herdr control operation was cancelled",
            ));
        }
        let identity = self.record.harness_anchor().ok_or_else(|| {
            AdapterError::recipient_changed(
                "adopted Muse record has no current foreground harness process anchor",
            )
        })?;
        let shell = self.record.foreign_shell_identity.as_ref().ok_or_else(|| {
            AdapterError::recipient_changed("adopted Muse record has no pane shell identity")
        })?;
        if self.client.pane_shell_identity(pane_id)? != *shell {
            return Err(AdapterError::recipient_changed(
                "adopted Muse pane shell process generation changed",
            ));
        }
        if !self.client.verify_harness_identity(pane_id, identity)? {
            return Err(AdapterError::recipient_changed(
                "adopted Muse foreground harness process generation changed",
            ));
        }
        Ok(())
    }

    /// State of a harness behind a root slot relay, from Herdr's live screen rules.
    ///
    /// Herdr reports the state agentctl last reported for such a pane, so the live verdict
    /// comes from `agent explain`, and a changed verdict is reported back so Herdr's own
    /// listing stays current.
    fn relay_pane_info(&self, mut info: AgentPaneInfo) -> crate::error::Result<AgentPaneInfo> {
        self.client
            .verify_relay_harness(&info.pane_id, self.record.custom_process_identity.as_ref())?;
        let (agent, state) = self.client.explain_agent(&info.pane_id)?;
        if agent.as_deref() != Some(self.record.harness.as_str()) {
            return Err(crate::error::AdapterError::unavailable(format!(
                "pane {} no longer shows a {} screen",
                info.pane_id, self.record.harness
            )));
        }
        if state == "blocked" {
            return Err(crate::error::AdapterError::unavailable(format!(
                "{} behind the slot relay is waiting for human attention; no input was submitted",
                self.record.harness
            )));
        }
        let mut state = state;
        if state != "working" {
            // Claude keeps its prompt box on screen while it works, and Herdr's rules then read
            // that box as idle; the interrupt hint is the tell.
            let screen = self.client.read(&info.pane_id, "visible", Some(200))?;
            if crate::client::relay_trust_prompt(&screen) {
                return Err(crate::error::AdapterError::unavailable(format!(
                    "{} workspace trust prompt requires human attention; no input was submitted",
                    self.record.harness
                )));
            }
            if screen.contains(RELAY_WORKING_MARKER) {
                state = "working".to_owned();
            }
        }
        if state != info.status
            && !(state == "idle" && info.status == "done")
            && matches!(state.as_str(), "idle" | "working" | "unknown")
        {
            self.client
                .report_pane_agent(&info.pane_id, &self.record.harness, &state)?;
        }
        info.agent = Some(self.record.harness.clone());
        info.status = state;
        Ok(info)
    }

    fn relay_wait(&self, pane_id: &str, status: &str, timeout_ms: u64) -> crate::error::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let observed = self.pane_info(pane_id)?.status;
            if observed == status {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(crate::error::AdapterError::unavailable(format!(
                    "relayed {} in pane {pane_id} did not become {status} within {timeout_ms} ms (last {observed})",
                    self.record.harness
                )));
            }
            std::thread::sleep(
                Duration::from_millis(200).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn relay_refuses_raw_input(&self, text: &str) -> crate::error::Result<()> {
        Err(crate::error::AdapterError::unavailable(
            if text.trim_start().starts_with('/') {
                "slash commands cannot be delivered to a harness behind a root slot relay"
            } else {
                "a harness behind a root slot relay takes only verified submissions"
            },
        ))
    }

    fn validate_workspace_policy(&self, info: &AgentPaneInfo) -> crate::error::Result<()> {
        let Some(expected) = self.expected_workspace.as_deref() else {
            return Ok(());
        };
        let actual = self.client.workspace_label(&info.workspace_id)?;
        if actual != expected {
            return Err(crate::error::AdapterError::unavailable(format!(
                "refusing pane {}: workspace is {actual:?}, expected {expected:?}",
                info.pane_id
            )));
        }
        Ok(())
    }
}

impl<'a, A: ManagedApi + ?Sized> WorkspaceClient<'a, A> {
    /// A terminal that verifies this record's recipient around every input effect.
    ///
    /// When the server advertises `input-expect` and the record pins a terminal, each write
    /// also carries that terminal so Herdr refuses it atomically if the pane holds another.
    fn guarded<'w>(
        &'w self,
        runtime: &'w dyn agent::AgentRuntime,
    ) -> crate::error::Result<GuardedTerminal<'w, 'a, A>> {
        let expect_terminal = match self.record.terminal_id.as_deref() {
            Some(terminal) => match self.client.input_expect_supported() {
                Ok(true) => Some(terminal.to_owned()),
                Ok(false) => None,
                Err(error) => {
                    return Err(AdapterError::not_staged(format!(
                    "cannot read the Herdr server's input capabilities: {error}; nothing was typed"
                )))
                }
            },
            None => None,
        };
        Ok(GuardedTerminal {
            workspace: self,
            runtime,
            expect_terminal,
            effects: Cell::new(0),
        })
    }

    /// Prove that the pane still holds this record's recipient; called around each effect.
    ///
    /// `presentation = false` skips only the tab label and the Herdr name lookup, for
    /// repairing them.
    fn verify_recipient(&self, pane_id: &str, presentation: bool) -> crate::error::Result<()> {
        let record = self.record;
        if Some(pane_id) != record.pane_id.as_deref() {
            return Err(AdapterError::recipient_changed(format!(
                "refusing input to pane {pane_id}: agent '{}' owns {}",
                record.name,
                record.pane_id.as_deref().unwrap_or("None")
            )));
        }
        let mut failures = Vec::new();
        if record.adapter == "herdr" && presentation {
            let identity = self.client.agent_identity(&record.name)?;
            if Some(identity.pane_id.as_str()) != record.pane_id.as_deref() {
                failures.push(format!(
                    "Herdr agent '{}' is in pane {}",
                    record.name, identity.pane_id
                ));
            }
        }
        let info = self.client.pane_info(pane_id)?;
        if Some(&info.workspace_id) != record.workspace_id.as_ref() {
            failures.push(format!(
                "workspace is '{}', recorded {}",
                info.workspace_id,
                repr(record.workspace_id.as_deref())
            ));
        }
        if !same_directory(&info.cwd, &record.cwd) {
            failures.push(format!("cwd is '{}', recorded '{}'", info.cwd, record.cwd));
        }
        if record.terminal_id.is_some() && info.terminal_id != record.terminal_id {
            failures.push(format!(
                "terminal is {}, recorded {}",
                repr(info.terminal_id.as_deref()),
                repr(record.terminal_id.as_deref())
            ));
        }
        if record.adapter != "herdr-foreign" {
            if let Some(recorded_tab) = record.tab_id.as_deref() {
                if info
                    .tab_id
                    .as_deref()
                    .is_some_and(|tab| tab != recorded_tab)
                {
                    failures.push(format!(
                        "tab is {}, recorded '{recorded_tab}'",
                        repr(info.tab_id.as_deref())
                    ));
                }
                let label = if presentation {
                    self.client.tab_label(recorded_tab)?
                } else {
                    record.name.clone()
                };
                if label != record.name {
                    failures.push(format!(
                        "tab label is '{label}', expected '{}'",
                        record.name
                    ));
                }
            }
        }
        // Flat schema-1 records carry only observed native sessions, never asserted ones.
        if !session_matches(record, &info) {
            failures.push(format!(
                "native session is {}/{}, recorded {}/{}",
                repr(info.session_agent.as_deref()),
                repr(info.session_value.as_deref()),
                repr(Some(
                    record.session_agent.as_deref().unwrap_or(&record.harness)
                )),
                repr(record.session_value.as_deref())
            ));
        }
        if !failures.is_empty() {
            return Err(AdapterError::recipient_changed(format!(
                "refusing input to agent '{}': {}; run `agentctl doctor`",
                record.name,
                failures.join("; ")
            )));
        }
        for (name, pane, terminal) in self.peer_claims()? {
            if pane == pane_id
                || (terminal.is_some() && terminal.as_deref() == info.terminal_id.as_deref())
            {
                return Err(AdapterError::recipient_changed(format!(
                    "refusing input to agent '{}': registered agent '{name}' also claims pane \
                     {pane_id}; run `agentctl doctor`",
                    record.name
                )));
            }
        }
        match record.adapter.as_str() {
            "herdr-pane" => {
                return self.client.verify_custom_harness(
                    pane_id,
                    &record.harness,
                    record.custom_process_identity.as_ref(),
                )
            }
            "herdr-relay" => {
                if record.custom_process_identity.is_none()
                    || self.client.relay_process(pane_id)? != record.custom_process_identity
                {
                    return Err(AdapterError::recipient_changed(format!(
                        "refusing input to agent '{}': its relayed harness process changed",
                        record.name
                    )));
                }
                return Ok(());
            }
            _ => {}
        }
        if self.uses_muse_composer() {
            return self.verify_muse_process(pane_id);
        }
        if let Some(identity) = record.harness_anchor() {
            if !self.client.verify_harness_identity(pane_id, identity)? {
                return Err(AdapterError::recipient_changed(format!(
                    "refusing input to agent '{}': the anchored {} process (pid {}) is no longer \
                     a foreground process of pane {pane_id}; run `agentctl doctor`",
                    record.name, record.harness, identity.pid
                )));
            }
            return Ok(());
        }
        if record.observed_session_value().is_some() {
            return Ok(());
        }
        Err(AdapterError::recipient_changed(format!(
            "refusing input to agent '{}': its record pins no harness process or observed native \
             session, so a replacement in the same pane could not be told apart; check that pane \
             {pane_id} runs the intended agent, then run `agentctl anchor {}`",
            record.name, record.name
        )))
    }

    /// Pane and terminal claims of every other active record, read conservatively.
    ///
    /// Each record is read as plain JSON, so a record this edition cannot fully decode still
    /// counts; one that is not readable JSON refuses input outright.
    fn peer_claims(&self) -> crate::error::Result<Vec<(String, String, Option<String>)>> {
        let Some(registry) = self.queue.and_then(Path::parent).and_then(Path::parent) else {
            return Ok(Vec::new());
        };
        registry_claims(registry, &self.record.name).map_err(AdapterError::unavailable)
    }
}

/// Does the pane still report the observed native session, provider and id, this record
/// holds? A record without one has nothing to compare.
fn session_matches(record: &AgentRecord, info: &AgentPaneInfo) -> bool {
    let Some(recorded) = record.observed_session_value() else {
        return true;
    };
    info.session_value.as_deref() == Some(recorded)
        && info.session_agent.as_deref()
            == Some(record.session_agent.as_deref().unwrap_or(&record.harness))
}

/// Do two paths name the same directory, resolving links when both exist?
fn same_directory(left: &str, right: &str) -> bool {
    let resolve = |path: &str| fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    resolve(left) == resolve(right)
}

/// Pane and terminal claims of every active record except `exclude`, read as plain JSON.
fn registry_claims(
    registry: &Path,
    exclude: &str,
) -> std::result::Result<Vec<(String, String, Option<String>)>, String> {
    let Ok(entries) = fs::read_dir(registry) else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name != "archive" && name != exclude && name_pattern(name))
        .collect();
    names.sort();
    let mut claims = Vec::new();
    for name in names {
        let path = registry.join(&name).join("agent.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot prove that pane ownership is unique: record '{name}' is unreadable \
                     ({error}); run `agentctl doctor`"
                ))
            }
        };
        let Ok(Value::Object(document)) = serde_json::from_slice::<Value>(&bytes) else {
            return Err(format!(
                "cannot prove that pane ownership is unique: record '{name}' is not a JSON \
                 object; run `agentctl doctor`"
            ));
        };
        if matches!(
            document.get("lifecycle").and_then(Value::as_str),
            Some("stopped" | "launch_failed")
        ) {
            continue;
        }
        if let Some(pane) = document.get("pane_id").and_then(Value::as_str) {
            claims.push((
                name,
                pane.to_owned(),
                document
                    .get("terminal_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ));
        }
    }
    for record in revive::pending_claim_records(registry).map_err(|error| error.to_string())? {
        if record.name != exclude {
            if let Some(pane) = record.pane_id {
                claims.push((record.name, pane, record.terminal_id));
            }
        }
    }
    Ok(claims)
}

/// Verifies the recipient immediately before and immediately after every input effect.
///
/// When the server advertises `input-expect` and the record pins a terminal, each write also
/// carries that terminal, and Herdr refuses it atomically if the pane holds another terminal.
/// Without that capability Herdr writes by pane id alone: a replacement that takes the pane
/// between the last check and Herdr's write still receives that one effect. Either way a
/// program exiting or exec-ing inside the same terminal is outside Herdr's pane table, so the
/// process check runs before and after every effect, and a mismatch after a write becomes a
/// quarantined probable misroute.
struct GuardedTerminal<'w, 'a, A: ManagedApi + ?Sized> {
    workspace: &'w WorkspaceClient<'a, A>,
    runtime: &'w dyn agent::AgentRuntime,
    expect_terminal: Option<String>,
    effects: Cell<u32>,
}

impl<A: ManagedApi + ?Sized> GuardedTerminal<'_, '_, A> {
    fn refused(&self, pane_id: &str, error: &AdapterError) -> AdapterError {
        if self.effects.get() == 0 {
            AdapterError::not_staged(format!("{error}; nothing was typed"))
        } else {
            AdapterError::recipient_changed(format!(
                "{error}; input already typed into pane {pane_id} stopped here"
            ))
        }
    }

    fn effect(
        &self,
        pane_id: &str,
        action: impl FnOnce() -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        if let Err(error) = self.workspace.verify_recipient(pane_id, true) {
            return Err(self.refused(pane_id, &error));
        }
        if let Err(error) = action() {
            if error.kind() == AdapterErrorKind::ExpectationFailed {
                return Err(self.refused(pane_id, &error));
            }
            // The write may have been accepted before the transport failed: check where it
            // could have gone before reporting an ambiguous outcome.
            self.effects.set(self.effects.get().saturating_add(1));
            return Err(match self.workspace.verify_recipient(pane_id, true) {
                Err(check) if check.kind() == AdapterErrorKind::RecipientChanged => {
                    AdapterError::probable_misroute(format!(
                        "pane {pane_id} failed its recipient check after an input whose outcome \
                         is unknown ({error}): {check}"
                    ))
                }
                _ => error,
            });
        }
        self.effects.set(self.effects.get().saturating_add(1));
        match self.workspace.verify_recipient(pane_id, true) {
            Err(error) if error.kind() == AdapterErrorKind::RecipientChanged => {
                Err(AdapterError::probable_misroute(format!(
                    "pane {pane_id} failed its recipient check immediately after input: {error}"
                )))
            }
            other => other,
        }
    }
}

impl<A: ManagedApi + ?Sized> GuardedInput for GuardedTerminal<'_, '_, A> {
    fn read_screen(&self, pane_id: &str) -> crate::error::Result<String> {
        self.workspace
            .client
            .read_screen_with_runtime(pane_id, self.runtime)
    }
    fn send_text(&self, pane_id: &str, text: &str) -> crate::error::Result<()> {
        self.effect(pane_id, || {
            self.workspace.client.send_text_expect(
                pane_id,
                text,
                self.expect_terminal.as_deref(),
                self.runtime,
            )
        })
    }
    fn send_keys(&self, pane_id: &str, keys: &str) -> crate::error::Result<()> {
        self.effect(pane_id, || {
            self.workspace.client.send_keys_expect(
                pane_id,
                keys,
                self.expect_terminal.as_deref(),
                self.runtime,
            )
        })
    }
    fn native_prompt(&self, pane_id: &str, text: &str) -> crate::error::Result<()> {
        self.effect(pane_id, || {
            self.workspace.client.agent_prompt_expect(
                pane_id,
                text,
                self.expect_terminal.as_deref(),
                self.runtime,
            )
        })
    }
}

/// Longest wait for a sent prompt to appear in the target pane's scrollback.
const READBACK: Duration = Duration::from_secs(5);

#[cfg(test)]
thread_local! {
    /// A test's own read-back limit, so a test that expects nothing to show does not hold
    /// shared pane locks for the full limit.
    pub(crate) static TEST_READBACK: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
    /// A test's own countermand opt-in, since the environment is shared by parallel tests.
    pub(crate) static TEST_COUNTERMAND: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
    /// A test's own wait for another pane's input lock.
    pub(crate) static TEST_COUNTERMAND_LOCK: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Whether a misroute is countermanded (Esc and a note) instead of only quarantined.
fn countermand_enabled() -> bool {
    #[cfg(test)]
    if let Some(enabled) = TEST_COUNTERMAND.with(std::cell::Cell::get) {
        return enabled;
    }
    std::env::var(COUNTERMAND_ENV).as_deref() == Ok("1")
}

/// How long a countermand waits for the wrong pane's input lock.
fn countermand_lock_limit() -> Duration {
    #[cfg(test)]
    if let Some(limit) = TEST_COUNTERMAND_LOCK.with(std::cell::Cell::get) {
        return limit;
    }
    COUNTERMAND_LOCK
}

/// The read-back limit in force.
fn readback_limit() -> Duration {
    #[cfg(test)]
    if let Some(limit) = TEST_READBACK.with(std::cell::Cell::get) {
        return limit;
    }
    READBACK
}
/// Shorter prompts are too common to attribute to another pane by their text.
const ATTRIBUTABLE: usize = 12;
/// Inline Markdown markup a harness may drop when it prints a submitted prompt (Claude shows
/// `code` without its backticks); read-back compares the prompt and the scrollback with it
/// removed from both.
const RENDERED_MARKUP: [char; 5] = ['`', '*', '_', '~', '\\'];

/// Text as read-back compares it: no whitespace and no droppable inline markup.
fn readback_form(text: &str) -> String {
    crate::submission::compact(text)
        .chars()
        .filter(|char| !RENDERED_MARKUP.contains(char))
        .collect()
}

/// The end of a prompt that read-back looks for in a pane.
fn readback_tail(text: &str) -> String {
    let chars: Vec<char> = readback_form(text).chars().collect();
    chars[chars.len().saturating_sub(40)..].iter().collect()
}
/// Longest wait for another pane's input lock before a countermand gives up.
const COUNTERMAND_LOCK: Duration = Duration::from_secs(2);
/// Lines of scrollback each read-back reads; a read this long may have lost old lines.
const SCROLLBACK_LINES: usize = 400;
/// Opt-in for countermanding a misroute; by default a misroute is only quarantined.
const COUNTERMAND_ENV: &str = "AGENTCTL_MISROUTE_COUNTERMAND";
/// What a program that received someone else's prompt is told after being interrupted.
pub const MISROUTE_NOTE: &str =
    "Ignore the previous message: it was sent to the wrong agent by agentctl.";
const NOTE_HARNESSES: [&str; 3] = ["claude", "codex", "muse"];

/// The doctor finding for a Herdr refusal naming a closed workspace, tab, or pane.
fn missing_target(error: &impl std::fmt::Display) -> Option<&'static str> {
    match crate::client::herdr_error_code(&error.to_string())?.as_str() {
        "workspace_not_found" => Some("workspace-missing"),
        "tab_not_found" => Some("tab-missing"),
        "pane_not_found" => Some("pane-missing"),
        _ => None,
    }
}

impl<A: ManagedApi + ?Sized> WorkspaceClient<'_, A> {
    /// Before sending: how often the prompt shows in the target and, for a prompt long enough
    /// to attribute, in every other registered agent's pane. Only occurrences beyond these are
    /// evidence about this send. A failure here happens before anything is typed, so it is not
    /// staged. A peer pane Herdr reports as not found cannot receive the prompt: it is logged
    /// and skipped. Any other failure, for the target or a peer, is a failure.
    fn snapshot(&self, pane_id: &str, text: &str) -> crate::error::Result<BTreeMap<String, usize>> {
        let mut panes = vec![pane_id.to_owned()];
        if readback_tail(text).chars().count() >= ATTRIBUTABLE {
            panes.extend(self.peer_panes().into_iter().filter(|peer| peer != pane_id));
        }
        let mut windows = BTreeMap::new();
        for pane in panes {
            match self.window(&pane, text) {
                Ok(window) => {
                    windows.insert(pane, window);
                }
                Err(error) => match missing_target(&error).filter(|_| pane != pane_id) {
                    Some(missing) => {
                        self.log_readback(
                            pane_id,
                            text,
                            "peer-skipped",
                            &json!({"peer": pane, "finding": missing, "detail": error.to_string()}),
                        );
                    }
                    None => {
                        return Err(AdapterError::not_staged(format!(
                            "cannot read scrollback before sending: {error}; nothing was typed"
                        )));
                    }
                },
            }
        }
        Ok(windows)
    }

    /// How often the text shows in the pane's scrollback.
    fn window(&self, pane: &str, text: &str) -> crate::error::Result<usize> {
        let screen = self.client.read_scrollback(pane)?;
        let tail = readback_tail(text);
        if tail.is_empty() {
            return Ok(0);
        }
        Ok(readback_form(&screen).matches(&tail).count())
    }

    /// How long the target has to show a sent prompt.
    fn readback_limit(&self) -> Duration {
        readback_limit().max(self.readback.unwrap_or_default())
    }

    /// `fresh` when the text shows more often than before. When it does not, a pane that
    /// already showed it is `uncertain`: an old occurrence may have scrolled or redrawn out as
    /// a new one came in, which equal counts cannot tell apart, whatever the window's length
    /// and even when the bytes read are unchanged. Otherwise `absent`.
    fn observe(
        &self,
        pane: &str,
        text: &str,
        before: &BTreeMap<String, usize>,
    ) -> crate::error::Result<&'static str> {
        let count = self.window(pane, text)?;
        let old_count = before.get(pane).copied().unwrap_or(0);
        Ok(if count > old_count {
            "fresh"
        } else if old_count > 0 {
            "uncertain"
        } else {
            "absent"
        })
    }

    /// Wait up to the read-back limit for the prompt to appear newly in the target pane.
    fn seen_in_target(
        &self,
        pane_id: &str,
        text: &str,
        before: &BTreeMap<String, usize>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<bool> {
        let deadline = runtime.monotonic() + self.readback_limit();
        loop {
            if self.observe(pane_id, text, before)? == "fresh" {
                return Ok(true);
            }
            let now = runtime.monotonic();
            if now >= deadline {
                return Ok(false);
            }
            runtime.sleep(Duration::from_millis(250).min(deadline.saturating_sub(now)));
        }
    }

    fn peer_states(
        &self,
        pane_id: &str,
        text: &str,
        before: &BTreeMap<String, usize>,
    ) -> crate::error::Result<BTreeMap<String, &'static str>> {
        let mut states = BTreeMap::new();
        for peer in before.keys().filter(|peer| peer.as_str() != pane_id) {
            states.insert(peer.clone(), self.observe(peer, text, before)?);
        }
        Ok(states)
    }

    /// After an unknown outcome: countermand a prompt that newly shows in exactly one other
    /// agent's pane and not in the target; otherwise only record what was seen. The caller
    /// reports the write as possibly submitted either way.
    fn located(
        &self,
        pane_id: &str,
        text: &str,
        before: &BTreeMap<String, usize>,
        detail: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        let in_target = self.seen_in_target(pane_id, text, before, runtime)?;
        let states = self.peer_states(pane_id, text, before)?;
        let fresh: Vec<&String> = states
            .iter()
            .filter(|(_, state)| **state == "fresh")
            .map(|(peer, _)| peer)
            .collect();
        if let ([other], false) = (fresh.as_slice(), in_target) {
            return Err(self.countermand(other, text, "prompt-in-another-pane", detail, runtime));
        }
        self.log_readback(
            pane_id,
            text,
            "unknown-outcome",
            &json!({"in_target": in_target, "peers": states, "detail": detail}),
        );
        Ok(())
    }

    /// Prove the prompt reached this record's pane, or countermand where it went.
    ///
    /// Evidence for the target: a receipt that saw the prompt printed there, or the prompt
    /// newly in its scrollback within the read-back limit ([`READBACK`], or the working timeout
    /// for a start brief). Every other registered agent's pane is
    /// inspected as well. The prompt newly in exactly one of them, and not in the target, is
    /// countermanded there. Newly in the target and a peer, in several peers, or a peer whose
    /// last occurrence moved nearer the end without a new one, cannot be told apart: the
    /// message is quarantined without a note. A verified submission that the target never
    /// shows is quarantined too; a native prompt goes on to Herdr's working-state
    /// confirmation, and is logged as not seen.
    fn read_back(
        &self,
        pane_id: &str,
        text: &str,
        submission: &Submission,
        before: &BTreeMap<String, usize>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        let recipient_holds = || -> crate::error::Result<()> {
            match self.verify_recipient(pane_id, true) {
                Ok(()) => Ok(()),
                // A check that could not be made is not evidence of a misroute.
                Err(error) if error.kind() != AdapterErrorKind::RecipientChanged => Err(error),
                Err(error) => Err(self.countermand(
                    pane_id,
                    text,
                    "identity-changed-after-write",
                    &error.to_string(),
                    runtime,
                )),
            }
        };
        recipient_holds()?;
        let in_target = matches!(submission, Submission::Verified(receipt) if receipt.printed)
            || self.seen_in_target(pane_id, text, before, runtime)?;
        recipient_holds()?;
        let states = self.peer_states(pane_id, text, before)?;
        let fresh: Vec<&String> = states
            .iter()
            .filter(|(_, state)| **state == "fresh")
            .map(|(peer, _)| peer)
            .collect();
        if let ([other], false) = (fresh.as_slice(), in_target) {
            return Err(self.countermand(
                other,
                text,
                "prompt-in-another-pane",
                &format!(
                    "not newly in pane {pane_id} after {}s",
                    self.readback_limit().as_secs()
                ),
                runtime,
            ));
        }
        if states.values().any(|state| *state != "absent") {
            self.log_readback(
                pane_id,
                text,
                "ambiguous",
                &json!({"in_target": in_target, "peers": states}),
            );
            return Err(AdapterError::probable_misroute(format!(
                "prompt for agent {} cannot be attributed: target {} it, other panes {}; \
                 quarantined without a note",
                repr(Some(&self.record.name)),
                if in_target { "shows" } else { "does not show" },
                json!(states)
            )));
        }
        if in_target {
            return Ok(());
        }
        // A confirmation without target evidence proves nothing about the recipient.
        let verified = matches!(submission, Submission::Verified(_));
        let logged = self.log_readback(pane_id, text, "not-seen", &json!({"receipt": verified}));
        Err(AdapterError::unavailable(format!(
            "prompt for agent {} was submitted but never showed in pane {pane_id} within {}s; \
             delivery is unproven{}",
            repr(Some(&self.record.name)),
            self.readback_limit().as_secs(),
            if logged {
                ""
            } else {
                " (the read-back log could not be written)"
            }
        )))
    }

    /// The program in `pane` now: its detected harness, terminal, and the harness process
    /// when it can be pinned.
    fn occupant(
        &self,
        pane: &str,
    ) -> crate::error::Result<(
        Option<String>,
        Option<String>,
        Option<CustomProcessIdentity>,
    )> {
        let info = self.client.pane_info(pane)?;
        let harness = match info.agent.as_deref() {
            Some(kind) if NOTE_HARNESSES.contains(&kind) => {
                self.client.harness_identity(pane, kind)?
            }
            _ => None,
        };
        Ok((info.agent, info.terminal_id, harness))
    }

    /// Interrupt the program that got this prompt and tell it to ignore it, once.
    ///
    /// The wrong pane's input lock is taken (within a bounded wait; this sender already holds
    /// its own target's lock) so no other agentctl input interleaves. The occupant seen now
    /// (terminal and harness process) is re-verified before the interrupt, before the note,
    /// and after it, and the note must newly show in that pane. A `MisrouteRecovered` error
    /// (one retry) needs all of that; anything less is `ProbableMisroute` (quarantine).
    fn countermand(
        &self,
        wrong_pane: &str,
        text: &str,
        detection: &str,
        detail: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> AdapterError {
        let mut observed_agent = None;
        let mut observed_terminal = None;
        let mut occupant = None;
        let mut interrupted = false;
        let mut note_sent = false;
        let mut note_confirmed = false;
        let mut skipped: Option<String> = None;
        let mut lock = None;
        if !countermand_enabled() {
            let logged = self.append_log(
                "misroutes.jsonl",
                &json!({
                    "at": unix_seconds(), "agent": self.record.name, "token": self.record.token,
                    "detection": detection, "detail": detail,
                    "intended_pane": self.record.pane_id,
                    "intended_terminal": self.record.terminal_id,
                    "observed_pane": wrong_pane, "interrupted": false, "note_sent": false,
                    "note_confirmed": false,
                    "skipped": format!(
                        "automatic countermand is off ({COUNTERMAND_ENV}=1 enables it); nothing typed"
                    ),
                    "message_id": self.inflight_message_id(text),
                }),
            );
            return AdapterError::probable_misroute(format!(
                "prompt for agent {} probably reached pane {wrong_pane} ({detection}); \
                 quarantined, nothing typed into any pane: {detail}{}",
                repr(Some(&self.record.name)),
                if logged {
                    ""
                } else {
                    " (the misroute log could not be written)"
                }
            ));
        }
        if Some(wrong_pane) != self.record.pane_id.as_deref() {
            match agent::lock_target_within(wrong_pane, "pane input lock", countermand_lock_limit())
            {
                Ok(held) => lock = Some(held),
                Err(error) => skipped = Some(format!("{error}; nothing typed")),
            }
        }
        if skipped.is_none() {
            match self.occupant(wrong_pane) {
                Ok((agent, terminal, harness)) => {
                    observed_agent = agent;
                    observed_terminal = terminal;
                    occupant = harness;
                }
                Err(error) => skipped = Some(format!("pane unavailable: {error}")),
            }
        }
        if skipped.is_none() && occupant.is_none() {
            skipped = Some(format!(
                "pane shows {} without one verifiable harness process; nothing typed",
                repr(observed_agent.as_deref())
            ));
        }
        let expect = match (&observed_terminal, self.client.input_expect_supported()) {
            (Some(terminal), Ok(true)) => Some(terminal.clone()),
            _ => None,
        };
        let same_occupant = || {
            self.occupant(wrong_pane)
                .map(|(_, terminal, harness)| terminal == observed_terminal && harness == occupant)
                .unwrap_or(false)
        };
        if skipped.is_none() {
            if !same_occupant() {
                skipped = Some("occupant changed before the interrupt; nothing typed".to_owned());
            } else if let Err(error) =
                self.client
                    .send_keys_expect(wrong_pane, "esc", expect.as_deref(), runtime)
            {
                skipped = Some(format!("recovery input failed: {error}"));
            } else {
                interrupted = true;
                runtime.sleep(Duration::from_millis(500));
                let note_before = self
                    .window(wrong_pane, MISROUTE_NOTE)
                    .map(|window| BTreeMap::from([(wrong_pane.to_owned(), window)]));
                match note_before {
                    Err(error) => skipped = Some(format!("recovery read failed: {error}")),
                    Ok(_) if !same_occupant() => {
                        skipped =
                            Some("occupant changed after the interrupt; note not sent".to_owned())
                    }
                    Ok(note_before) => match self.client.agent_prompt_expect(
                        wrong_pane,
                        MISROUTE_NOTE,
                        expect.as_deref(),
                        runtime,
                    ) {
                        Err(error) => skipped = Some(format!("recovery input failed: {error}")),
                        Ok(()) => {
                            note_sent = true;
                            let deadline = runtime.monotonic() + readback_limit();
                            loop {
                                note_confirmed = matches!(
                                    self.observe(wrong_pane, MISROUTE_NOTE, &note_before),
                                    Ok("fresh")
                                ) && same_occupant();
                                let now = runtime.monotonic();
                                if note_confirmed || now >= deadline {
                                    break;
                                }
                                runtime.sleep(
                                    Duration::from_millis(250).min(deadline.saturating_sub(now)),
                                );
                            }
                            if !note_confirmed {
                                skipped = Some(
                                    "the note did not show in that pane for the same occupant; \
                                     its recipient is unproven"
                                        .to_owned(),
                                );
                            }
                        }
                    },
                }
            }
        }
        drop(lock);
        let logged = self.append_log(
            "misroutes.jsonl",
            &json!({
                "at": unix_seconds(), "agent": self.record.name, "token": self.record.token,
                "detection": detection, "detail": detail,
                "intended_pane": self.record.pane_id, "intended_terminal": self.record.terminal_id,
                "observed_pane": wrong_pane, "observed_terminal": observed_terminal,
                "observed_agent": observed_agent, "interrupted": interrupted,
                "note_sent": note_sent, "note_confirmed": note_confirmed, "skipped": skipped,
                "message_id": self.inflight_message_id(text),
            }),
        );
        if note_confirmed && logged {
            return AdapterError::misroute_recovered(format!(
                "prompt for agent {} reached pane {wrong_pane} ({detection}); that program was \
                 interrupted and told to ignore it: {detail}",
                repr(Some(&self.record.name))
            ));
        }
        AdapterError::probable_misroute(format!(
            "prompt for agent {} probably reached pane {wrong_pane} ({detection}) and was not \
             proven countermanded ({}): {detail}{}",
            repr(Some(&self.record.name)),
            skipped.unwrap_or_default(),
            if logged {
                ""
            } else {
                " (the misroute log could not be written)"
            }
        ))
    }

    /// Press Enter on a goal-replacement menu; a misroute is countermanded like a prompt's.
    fn confirm_goal_replacement(
        &self,
        pane_id: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        match self.guarded(runtime)?.send_keys(pane_id, "Enter") {
            Err(error) if error.kind() == AdapterErrorKind::ProbableMisroute => Err(self
                .countermand(
                    pane_id,
                    "Enter",
                    "identity-changed-after-write",
                    &format!("goal confirmation: {error}"),
                    runtime,
                )),
            other => other,
        }
    }

    /// The message being delivered: its inflight file, matched by text or, since the
    /// inflight barrier holds one message at a time, the only one present.
    fn inflight_message_id(&self, text: &str) -> Option<String> {
        let inflight = self.queue?.join("inflight");
        let mut paths: Vec<PathBuf> = fs::read_dir(inflight)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        paths.sort();
        let stem = |path: &PathBuf| path.file_stem()?.to_str().map(str::to_owned);
        paths
            .iter()
            .find(|path| {
                fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .is_some_and(|document| {
                        document.get("text").and_then(Value::as_str) == Some(text)
                    })
            })
            .and_then(stem)
            .or_else(|| match paths.as_slice() {
                [only] => stem(only),
                _ => None,
            })
    }

    /// Append one fsynced line to a log beside the queue; false when it failed.
    fn append_log(&self, filename: &str, entry: &Value) -> bool {
        let Some(directory) = self.queue.and_then(Path::parent) else {
            return false;
        };
        let opened = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(filename));
        let Ok(mut file) = opened else {
            return false;
        };
        let mut line = entry.to_string();
        line.push('\n');
        file.write_all(line.as_bytes()).is_ok() && file.sync_all().is_ok()
    }

    fn log_readback(&self, pane_id: &str, text: &str, result: &str, evidence: &Value) -> bool {
        self.append_log(
            "readback.jsonl",
            &json!({
                "at": unix_seconds(), "agent": self.record.name, "pane": pane_id,
                "result": result, "evidence": evidence,
                "message_id": self.inflight_message_id(text),
            }),
        )
    }

    /// Panes of every other running record in this registry, for locating a misroute.
    fn peer_panes(&self) -> Vec<String> {
        let Some(registry) = self.queue.and_then(Path::parent).and_then(Path::parent) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(registry) else {
            return Vec::new();
        };
        let mut panes: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                if name == "archive" || name == self.record.name || !name_pattern(&name) {
                    return None;
                }
                let document: Value =
                    serde_json::from_slice(&fs::read(entry.path().join("agent.json")).ok()?)
                        .ok()?;
                (document.get("lifecycle").and_then(Value::as_str) == Some("running"))
                    .then(|| document.get("pane_id")?.as_str().map(str::to_owned))
                    .flatten()
            })
            .collect();
        panes.sort();
        panes
    }

    fn submit_unchecked(
        &self,
        pane_id: &str,
        text: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Submission> {
        // Custom panes verify through their own composer model, and slash
        // commands (including goal replacement) need the native primitive.
        if self.uses_muse_composer() || text.trim_start().starts_with('/') {
            return self
                .run_with_runtime(pane_id, text, runtime)
                .map(|()| Submission::Unconfirmed);
        }
        if runtime.cancelled() {
            return Ok(Submission::NotStaged(
                "delivery was cancelled before typing".to_owned(),
            ));
        }
        *self
            .goal_objective
            .lock()
            .expect("goal operation lock poisoned") = None;
        if self.record.adapter == "herdr-relay" {
            let status = self.pane_info_with_runtime(pane_id, runtime)?.status;
            if status != "idle" {
                return Ok(Submission::NotStaged(format!(
                    "relayed {} in pane {pane_id} is {status}, not idle",
                    self.record.harness
                )));
            }
            let guarded = self.guarded(runtime)?;
            return self.client.submit_guarded(
                pane_id,
                &self.record.harness,
                text,
                &guarded,
                runtime,
            );
        }
        let guarded = self.guarded(runtime)?;
        self.client.prompt_guarded(pane_id, text, &guarded, runtime)
    }
}

impl<A: ManagedApi + ?Sized> WorkspaceClient<'_, A> {
    fn wait_agent_status_unchecked(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
    ) -> crate::error::Result<()> {
        if self.record.adapter == "herdr-relay" {
            return self.relay_wait(pane_id, status, timeout_ms);
        }
        if self.uses_muse_composer() && status == "working" {
            let submission = self
                .custom_submission
                .lock()
                .expect("custom submission lock poisoned")
                .clone()
                .ok_or_else(|| {
                    crate::error::AdapterError::unavailable(
                        "custom pane harness has no pending submission receipt",
                    )
                })?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                self.verify_muse_process(pane_id)?;
                let screen = self.client.read(pane_id, "visible", Some(200))?;
                self.verify_muse_process(pane_id)?;
                if screen != submission.staged_screen
                    && muse_prompt_transcript_count(&screen, &submission.text)
                        > submission.prior_transcript_count
                    && !muse_prompt_in_composer(&screen, &submission.text)
                {
                    *self
                        .custom_submission
                        .lock()
                        .expect("custom submission lock poisoned") = None;
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(crate::error::AdapterError::unavailable(
                        "Muse did not show a verified post-Enter screen transition",
                    ));
                }
                std::thread::sleep(
                    Duration::from_millis(50)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        let Some(objective) = self
            .goal_objective
            .lock()
            .expect("goal operation lock poisoned")
            .clone()
            .filter(|_| status == "working")
        else {
            return self.client.wait_agent_status(pane_id, status, timeout_ms);
        };
        let start = Instant::now();
        if self
            .client
            .wait_agent_status(pane_id, status, timeout_ms.min(1000))
            .is_ok()
        {
            return Ok(());
        }
        self.pane_info(pane_id)?;
        let screen = self.client.read(pane_id, "visible", Some(200))?;
        if goal_replacement_selected(&screen, &objective) {
            let runtime = agent::SystemRuntime::default();
            self.confirm_goal_replacement(pane_id, &runtime)?;
        }
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.client
            .wait_agent_status(pane_id, status, timeout_ms.saturating_sub(elapsed).max(1))
    }

    fn wait_agent_status_with_runtime_unchecked(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        if self.record.adapter == "herdr-relay" {
            let _ = runtime;
            return self.relay_wait(pane_id, status, timeout_ms);
        }
        if self.uses_muse_composer() && status == "working" {
            let submission = self
                .custom_submission
                .lock()
                .expect("custom submission lock poisoned")
                .clone()
                .ok_or_else(|| {
                    crate::error::AdapterError::unavailable(
                        "custom pane harness has no pending submission receipt",
                    )
                })?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                if runtime.cancelled() {
                    return Err(crate::error::AdapterError::unavailable(
                        "Herdr control operation was cancelled",
                    ));
                }
                self.verify_muse_process_with_runtime(pane_id, runtime)?;
                let screen =
                    self.client
                        .read_with_runtime(pane_id, "visible", Some(200), runtime)?;
                self.verify_muse_process_with_runtime(pane_id, runtime)?;
                if screen != submission.staged_screen
                    && muse_prompt_transcript_count(&screen, &submission.text)
                        > submission.prior_transcript_count
                    && !muse_prompt_in_composer(&screen, &submission.text)
                {
                    *self
                        .custom_submission
                        .lock()
                        .expect("custom submission lock poisoned") = None;
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(crate::error::AdapterError::unavailable(
                        "Muse did not show a verified post-Enter screen transition",
                    ));
                }
                std::thread::sleep(
                    Duration::from_millis(50)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        let Some(objective) = self
            .goal_objective
            .lock()
            .expect("goal operation lock poisoned")
            .clone()
            .filter(|_| status == "working")
        else {
            return self
                .client
                .wait_agent_status_with_runtime(pane_id, status, timeout_ms, runtime);
        };
        let start = Instant::now();
        if self
            .client
            .wait_agent_status_with_runtime(pane_id, status, timeout_ms.min(1000), runtime)
            .is_ok()
        {
            return Ok(());
        }
        self.pane_info_with_runtime(pane_id, runtime)?;
        let screen = self
            .client
            .read_with_runtime(pane_id, "visible", Some(200), runtime)?;
        if goal_replacement_selected(&screen, &objective) {
            self.confirm_goal_replacement(pane_id, runtime)?;
        }
        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.client.wait_agent_status_with_runtime(
            pane_id,
            status,
            timeout_ms.saturating_sub(elapsed).max(1),
            runtime,
        )
    }
}

impl<A: ManagedApi + ?Sized> AgentApi for WorkspaceClient<'_, A> {
    fn codex_idle_ready_with_runtime(
        &self,
        info: &AgentPaneInfo,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<bool> {
        if self.record.harness != "codex"
            || info.status != "unknown"
            || info.agent.as_deref() != Some("codex")
            || Some(info.pane_id.as_str()) != self.record.pane_id.as_deref()
        {
            return Ok(false);
        }
        self.verify_recipient(&info.pane_id, true)?;
        let ready = self.client.codex_idle_ready_with_runtime(info, runtime)?;
        self.verify_recipient(&info.pane_id, true)?;
        Ok(ready)
    }

    fn panes(&self) -> crate::error::Result<Vec<Pane>> {
        Ok(self
            .client
            .panes()?
            .into_iter()
            .filter(|pane| Some(&pane.workspace_id) == self.record.workspace_id.as_ref())
            .collect())
    }
    fn pane_info(&self, pane_id: &str) -> crate::error::Result<AgentPaneInfo> {
        if self.record.adapter == "herdr"
            && Some(self.client.agent_pane(&self.record.name)?) != self.record.pane_id
        {
            return Err(crate::error::AdapterError::unavailable(format!(
                "agent {:?} no longer owns its recorded pane",
                self.record.name
            )));
        }
        let mut info = self.client.pane_info(pane_id)?;
        if Some(&info.workspace_id) != self.record.workspace_id.as_ref() {
            return Err(crate::error::AdapterError::unavailable(format!(
                "agent {:?} workspace identity changed",
                self.record.name
            )));
        }
        self.validate_workspace_policy(&info)?;
        if Some(pane_id) == self.record.pane_id.as_deref() {
            if self.record.goal_session_id.is_some()
                && info.session_value.is_some()
                && info.session_value != self.record.goal_session_id
            {
                return Err(crate::error::AdapterError::unavailable(format!(
                    "agent {:?} native session identity changed",
                    self.record.name
                )));
            }
            if self.check_prompt
                && matches!(info.status.as_str(), "idle" | "done")
                && info.agent.as_deref() == Some("claude")
            {
                let screen = self.client.read(pane_id, "visible", Some(200))?;
                if screen
                    .contains("Quick safety check: Is this a project you created or one you trust?")
                    && screen.contains("No, exit")
                    && screen.contains("Yes, I trust this folder")
                {
                    return Err(crate::error::AdapterError::unavailable("Claude workspace trust prompt requires human attention; no input was submitted"));
                }
            }
            if self.record.adapter == "herdr-relay" {
                return self.relay_pane_info(info);
            }
            if self.uses_muse_composer() {
                self.verify_muse_process(pane_id)?;
                let screen = self.client.read(pane_id, "visible", Some(200))?;
                self.verify_muse_process(pane_id)?;
                if muse_trust_prompt(&screen) {
                    return Err(crate::error::AdapterError::unavailable(
                        "Muse workspace trust prompt requires human attention; no input was submitted",
                    ));
                }
                info.status = muse_verified_pane_status(&info.status, &screen).to_owned();
            }
        }
        Ok(info)
    }
    fn workspace_label(&self, workspace_id: &str) -> crate::error::Result<String> {
        self.client.workspace_label(workspace_id)
    }
    fn run(&self, pane_id: &str, text: &str) -> crate::error::Result<()> {
        *self
            .goal_objective
            .lock()
            .expect("goal operation lock poisoned") = None;
        if let Some(queue) = self.queue {
            for (identifier, objective) in &self.record.goal_messages {
                let path = queue.join("inflight").join(format!("{identifier}.json"));
                if text == format!("/goal {objective}") && fs::symlink_metadata(&path).is_ok() {
                    let document = agent::read_private_json(&path).map_err(|error| {
                        crate::error::AdapterError::unavailable(error.to_string())
                    })?;
                    if document["text"].as_str() == Some(text) {
                        *self
                            .goal_objective
                            .lock()
                            .expect("goal operation lock poisoned") = Some(objective.clone());
                        break;
                    }
                }
            }
        }
        if self.record.adapter == "herdr-relay" {
            return self.relay_refuses_raw_input(text);
        }
        let runtime = agent::SystemRuntime::default();
        let guarded = self.guarded(&runtime)?;
        if !self.uses_muse_composer() {
            return guarded.native_prompt(pane_id, text);
        }
        if text.contains(['\0', '\u{1b}']) {
            return Err(crate::error::AdapterError::unavailable(
                "Muse pane prompts cannot contain NUL or terminal escape characters",
            ));
        }
        let info = self.pane_info(pane_id)?;
        if info.status != "idle" {
            return Err(crate::error::AdapterError::unavailable(format!(
                "custom pane {pane_id} is not at a verified idle Muse composer"
            )));
        }
        let before = self.client.read(pane_id, "visible", Some(200))?;
        if !muse_auto_review_idle_composer(&before) {
            return Err(AdapterError::unavailable(
                "Muse input requires the legacy reviewed Auto-review composer; current YOLO input remains pending until Herdr provides an effect-coupled process-generation guard",
            ));
        }
        guarded.send_text(
            pane_id,
            &format!("{BRACKETED_PASTE_START}{text}{BRACKETED_PASTE_END}"),
        )?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let staged = loop {
            self.verify_muse_process(pane_id)?;
            let screen = self.client.read(pane_id, "visible", Some(200))?;
            self.verify_muse_process(pane_id)?;
            if screen != before && muse_prompt_is_exact_composer(&screen, text) {
                break screen;
            }
            if Instant::now() >= deadline {
                return Err(crate::error::AdapterError::unavailable(
                    "literal text insertion did not produce exact visible Muse editor evidence; Enter was not sent",
                ));
            }
            std::thread::sleep(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            );
        };
        // The guard verifies the pinned Muse process immediately around the Enter.
        guarded.send_keys(pane_id, "Enter")?;
        *self
            .custom_submission
            .lock()
            .expect("custom submission lock poisoned") = Some(CustomPaneSubmission {
            prior_transcript_count: muse_prompt_transcript_count(&staged, text),
            staged_screen: staged,
            text: text.to_owned(),
        });
        Ok(())
    }
    fn wait_agent_status(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
    ) -> crate::error::Result<()> {
        self.wait_agent_status_unchecked(pane_id, status, timeout_ms)?;
        // A replacement's state must never confirm delivery to the recorded recipient.
        if status == "working" {
            self.verify_recipient(pane_id, true)?;
        }
        Ok(())
    }
    fn read(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
    ) -> crate::error::Result<String> {
        self.client.read(pane_id, source, lines)
    }

    fn panes_with_runtime(
        &self,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Vec<Pane>> {
        Ok(self
            .client
            .panes_with_runtime(runtime)?
            .into_iter()
            .filter(|pane| Some(&pane.workspace_id) == self.record.workspace_id.as_ref())
            .collect())
    }

    fn pane_info_with_runtime(
        &self,
        pane_id: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<AgentPaneInfo> {
        if self.record.adapter == "herdr"
            && Some(
                self.client
                    .agent_pane_with_runtime(&self.record.name, runtime)?,
            ) != self.record.pane_id
        {
            return Err(crate::error::AdapterError::unavailable(format!(
                "agent {:?} no longer owns its recorded pane",
                self.record.name
            )));
        }
        let mut info = self.client.pane_info_with_runtime(pane_id, runtime)?;
        if Some(&info.workspace_id) != self.record.workspace_id.as_ref() {
            return Err(crate::error::AdapterError::unavailable(format!(
                "agent {:?} workspace identity changed",
                self.record.name
            )));
        }
        self.validate_workspace_policy(&info)?;
        if Some(pane_id) == self.record.pane_id.as_deref() {
            if self.record.goal_session_id.is_some()
                && info.session_value.is_some()
                && info.session_value != self.record.goal_session_id
            {
                return Err(crate::error::AdapterError::unavailable(format!(
                    "agent {:?} native session identity changed",
                    self.record.name
                )));
            }
            if self.check_prompt
                && matches!(info.status.as_str(), "idle" | "done")
                && info.agent.as_deref() == Some("claude")
            {
                let screen =
                    self.client
                        .read_with_runtime(pane_id, "visible", Some(200), runtime)?;
                if screen
                    .contains("Quick safety check: Is this a project you created or one you trust?")
                    && screen.contains("No, exit")
                    && screen.contains("Yes, I trust this folder")
                {
                    return Err(crate::error::AdapterError::unavailable("Claude workspace trust prompt requires human attention; no input was submitted"));
                }
            }
            if self.record.adapter == "herdr-relay" {
                return self.relay_pane_info(info);
            }
            if self.uses_muse_composer() {
                self.verify_muse_process_with_runtime(pane_id, runtime)?;
                let screen =
                    self.client
                        .read_with_runtime(pane_id, "visible", Some(200), runtime)?;
                self.verify_muse_process_with_runtime(pane_id, runtime)?;
                if muse_trust_prompt(&screen) {
                    return Err(crate::error::AdapterError::unavailable(
                        "Muse workspace trust prompt requires human attention; no input was submitted",
                    ));
                }
                info.status = muse_verified_pane_status(&info.status, &screen).to_owned();
            }
        }
        Ok(info)
    }

    fn workspace_label_with_runtime(
        &self,
        workspace_id: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<String> {
        self.client
            .workspace_label_with_runtime(workspace_id, runtime)
    }

    fn run_with_runtime(
        &self,
        pane_id: &str,
        text: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        if runtime.cancelled() {
            return Err(crate::error::AdapterError::unavailable(
                "Herdr control operation was cancelled",
            ));
        }
        *self
            .goal_objective
            .lock()
            .expect("goal operation lock poisoned") = None;
        if let Some(queue) = self.queue {
            for (identifier, objective) in &self.record.goal_messages {
                let path = queue.join("inflight").join(format!("{identifier}.json"));
                if text == format!("/goal {objective}") && fs::symlink_metadata(&path).is_ok() {
                    let document = agent::read_private_json(&path).map_err(|error| {
                        crate::error::AdapterError::unavailable(error.to_string())
                    })?;
                    if document["text"].as_str() == Some(text) {
                        *self
                            .goal_objective
                            .lock()
                            .expect("goal operation lock poisoned") = Some(objective.clone());
                        break;
                    }
                }
            }
        }
        if self.record.adapter == "herdr-relay" {
            return self.relay_refuses_raw_input(text);
        }
        let guarded = self.guarded(runtime)?;
        if !self.uses_muse_composer() {
            return guarded.native_prompt(pane_id, text);
        }
        if text.contains(['\0', '\u{1b}']) {
            return Err(crate::error::AdapterError::unavailable(
                "Muse pane prompts cannot contain NUL or terminal escape characters",
            ));
        }
        let info = self.pane_info_with_runtime(pane_id, runtime)?;
        if info.status != "idle" {
            return Err(crate::error::AdapterError::unavailable(format!(
                "custom pane {pane_id} is not at a verified idle Muse composer"
            )));
        }
        let before = self
            .client
            .read_with_runtime(pane_id, "visible", Some(200), runtime)?;
        if !muse_auto_review_idle_composer(&before) {
            return Err(AdapterError::unavailable(
                "Muse input requires the legacy reviewed Auto-review composer; current YOLO input remains pending until Herdr provides an effect-coupled process-generation guard",
            ));
        }
        guarded.send_text(
            pane_id,
            &format!("{BRACKETED_PASTE_START}{text}{BRACKETED_PASTE_END}"),
        )?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let staged = loop {
            if runtime.cancelled() {
                return Err(crate::error::AdapterError::unavailable(
                    "Herdr control operation was cancelled",
                ));
            }
            self.verify_muse_process_with_runtime(pane_id, runtime)?;
            let screen = self
                .client
                .read_with_runtime(pane_id, "visible", Some(200), runtime)?;
            self.verify_muse_process_with_runtime(pane_id, runtime)?;
            if screen != before && muse_prompt_is_exact_composer(&screen, text) {
                break screen;
            }
            if Instant::now() >= deadline {
                return Err(crate::error::AdapterError::unavailable(
                    "literal text insertion did not produce exact visible Muse editor evidence; Enter was not sent",
                ));
            }
            std::thread::sleep(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            );
        };
        // The guard verifies the pinned Muse process immediately around the Enter.
        guarded.send_keys(pane_id, "Enter")?;
        *self
            .custom_submission
            .lock()
            .expect("custom submission lock poisoned") = Some(CustomPaneSubmission {
            prior_transcript_count: muse_prompt_transcript_count(&staged, text),
            staged_screen: staged,
            text: text.to_owned(),
        });
        Ok(())
    }

    fn submit_with_runtime(
        &self,
        pane_id: &str,
        text: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<Submission> {
        if !matches!(
            self.record.adapter.as_str(),
            "herdr" | "herdr-foreign" | "herdr-relay"
        ) {
            return self.submit_unchecked(pane_id, text, runtime);
        }
        let before = self.snapshot(pane_id, text)?;
        match self.submit_unchecked(pane_id, text, runtime) {
            Err(error) if error.kind() == AdapterErrorKind::ProbableMisroute => Err(self
                .countermand(
                    pane_id,
                    text,
                    "identity-changed-after-write",
                    &error.to_string(),
                    runtime,
                )),
            Err(error)
                if matches!(
                    error.kind(),
                    AdapterErrorKind::NotStaged | AdapterErrorKind::ExpectationFailed
                ) =>
            {
                Err(error)
            }
            // The outcome of a write is unknown (lost acknowledgement, composer failure).
            // Text in the target is no proof of submission (an unsubmitted paste shows in
            // the composer), so it stays possibly submitted; the read-back only looks for the
            // prompt in another agent's pane.
            Err(error) => {
                self.located(pane_id, text, &before, &error.to_string(), runtime)?;
                Err(error)
            }
            Ok(Submission::NotStaged(reason)) => Ok(Submission::NotStaged(reason)),
            Ok(submission) => {
                self.read_back(pane_id, text, &submission, &before, runtime)?;
                Ok(submission)
            }
        }
    }

    fn wait_agent_status_with_runtime(
        &self,
        pane_id: &str,
        status: &str,
        timeout_ms: u64,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<()> {
        self.wait_agent_status_with_runtime_unchecked(pane_id, status, timeout_ms, runtime)?;
        // A replacement's state must never confirm delivery to the recorded recipient.
        if status == "working" {
            self.verify_recipient(pane_id, true)?;
        }
        Ok(())
    }

    fn read_with_runtime(
        &self,
        pane_id: &str,
        source: &str,
        lines: Option<usize>,
        runtime: &dyn agent::AgentRuntime,
    ) -> crate::error::Result<String> {
        self.client
            .read_with_runtime(pane_id, source, lines, runtime)
    }
}

/// Options for a new visible subagent; the working directory is always explicit.
#[derive(Clone, Debug)]
pub struct StartOptions {
    /// Existing workspace ID, or the current workspace / shared `subagents` default.
    pub workspace_id: Option<String>,
    /// Owner-configured launch profile selected by the caller, retained for recovery.
    pub profile: Option<String>,
    /// Herdr agent kind.
    pub harness: String,
    /// Optional model override for Codex or Claude.
    pub model: Option<String>,
    /// Optional saved conversation to resume.
    pub resume: Option<String>,
    /// Additional literal harness arguments.
    pub harness_args: Vec<String>,
    /// Literal environment entries for the new Herdr tab.
    pub environment: Vec<String>,
    /// Initial task to submit after the harness is ready.
    pub brief: Option<String>,
    /// Herdr's bounded startup-readiness deadline.
    pub startup_timeout: Duration,
    /// Delivery options for the initial task.
    pub delivery: DrainOptions,
    /// Agentcloud settings; required shape for, and only accepted with, harness `agentcloud`.
    pub cloud: Option<CloudLaunch>,
    /// Box the interactive agent to one wrkslots slot before the harness starts.
    pub slot: Option<SlotLaunch>,
}

/// The wrkslots slot an interactive agent is boxed to (`agentctl start --slot`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SlotLaunch {
    /// Registered slot name.
    pub slot: String,
    /// `userns`, `cgroup`, or `root`; `None` uses the project's configured isolation.
    pub isolation: Option<String>,
    /// Explicit wrkslots project root; `None` searches upward from the agent cwd.
    pub project: Option<PathBuf>,
    /// Explicit wrkslots executable; `None` uses `AGENTCTL_WRKSLOTS_BIN`, else `PATH`.
    pub executable: Option<PathBuf>,
    /// A coordinator box (`agentctl start --project-box`) instead of a slot's box: `slot` is
    /// then the box name, and the registry and every slot are writable inside it.
    pub project_box: bool,
    /// With `project_box`: `worktrees` or `project`; `None` uses the project's configured scope.
    pub box_writable: Option<String>,
    /// With `project_box`: the box's working directory (the agent's `--cwd`).
    pub box_cwd: Option<PathBuf>,
}

/// What a coordinator box may write besides the usual paths (`wrkslots box --writable`).
pub const BOX_SCOPES: [&str; 2] = ["worktrees", "project"];

/// Isolation modes `wrkslots run` accepts.
pub const SLOT_ISOLATIONS: [&str; 3] = ["userns", "cgroup", "root"];

/// Harnesses agentctl can drive behind a root slot relay: Herdr has screen rules for them and
/// agentctl has verified composer models.
pub const RELAY_HARNESSES: [&str; 2] = ["claude", "codex"];

/// Both relayed harnesses show this hint only while a turn is running.
const RELAY_WORKING_MARKER: &str = "esc to interrupt";

/// Environment variable naming the wrkslots executable, overriding `PATH`.
pub const WRKSLOTS_BIN_ENV: &str = "AGENTCTL_WRKSLOTS_BIN";

fn wrkslots_executable() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(WRKSLOTS_BIN_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join("wrkslots"))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

/// Ask the slot manager for the exec-only command line that boxes a pane shell.
///
/// Returns the command line, the slot directory (the agent's working directory), and the
/// effective isolation. Runs `wrkslots [--project-root DIR] shell-command SLOT
/// [--isolation MODE] --format json [-- COMMAND...]` in `project`, bounded at 60 seconds; a
/// `command` replaces the interactive shell in the line. For a coordinator box it runs
/// `shell-command --box --name NAME --cwd DIR [--writable SCOPE]` instead, and the returned
/// directory is the box's working directory.
pub fn slot_shell_command(
    launch: &SlotLaunch,
    project: &Path,
    command_argv: &[String],
) -> Result<(String, PathBuf, String)> {
    let flag = if launch.project_box {
        "--box-isolation"
    } else {
        "--slot-isolation"
    };
    if let Some(isolation) = launch.isolation.as_deref() {
        if !SLOT_ISOLATIONS.contains(&isolation) {
            return Err(fail(format!("{flag} must be userns, cgroup, or root")));
        }
    }
    if let Some(scope) = launch.box_writable.as_deref() {
        if !BOX_SCOPES.contains(&scope) {
            return Err(fail("--box-writable must be worktrees or project"));
        }
    }
    let executable = launch
        .executable
        .clone()
        .or_else(wrkslots_executable)
        .ok_or_else(|| {
            fail(if launch.project_box {
                "--project-box needs wrkslots on PATH (or AGENTCTL_WRKSLOTS_BIN naming it)"
            } else {
                "--slot needs wrkslots on PATH (or AGENTCTL_WRKSLOTS_BIN naming it)"
            })
        })?;
    let slot = launch.slot.as_str();
    let mut command = std::process::Command::new(&executable);
    if launch.project.is_some() {
        command.arg("--project-root").arg(project);
    }
    if launch.project_box {
        command.args(["shell-command", "--box", "--name", slot]);
        if let Some(cwd) = launch.box_cwd.as_ref() {
            command.arg("--cwd").arg(cwd);
        }
        if let Some(scope) = launch.box_writable.as_deref() {
            command.args(["--writable", scope]);
        }
    } else {
        command.args(["shell-command", slot]);
    }
    if let Some(isolation) = launch.isolation.as_deref() {
        command.args(["--isolation", isolation]);
    }
    command.args(["--format", "json"]);
    if !command_argv.is_empty() {
        command.arg("--").args(command_argv);
    }
    command
        .current_dir(project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| fail(format!("wrkslots shell-command '{slot}' failed: {error}")))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!(
                    "wrkslots shell-command '{slot}' failed: timed out after 60s"
                )));
            }
            Err(error) => {
                return Err(fail(format!(
                    "wrkslots shell-command '{slot}' failed: {error}"
                )))
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| fail(format!("wrkslots shell-command '{slot}' failed: {error}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        let detail = if detail.is_empty() {
            format!("exit {}", output.status.code().unwrap_or(-1))
        } else {
            detail.to_owned()
        };
        return Err(fail(format!(
            "wrkslots shell-command '{slot}' failed: {detail}"
        )));
    }
    let invalid = |reason: &str| {
        fail(format!(
            "wrkslots shell-command '{slot}' returned invalid JSON: {reason}"
        ))
    };
    let document: Value =
        serde_json::from_str(stdout.trim()).map_err(|error| invalid(&error.to_string()))?;
    let object = document
        .as_object()
        .ok_or_else(|| invalid("not an object"))?;
    let line = object
        .get("command")
        .and_then(Value::as_str)
        .filter(|line| !line.is_empty());
    let slot_path = object.get("slot_path").and_then(Value::as_str);
    let (Some(line), Some(slot_path)) = (line, slot_path) else {
        return Err(invalid("missing fields"));
    };
    let effective = object
        .get("isolation")
        .and_then(Value::as_str)
        .or(launch.isolation.as_deref())
        .unwrap_or("userns");
    if !SLOT_ISOLATIONS.contains(&effective) {
        return Err(invalid(&format!("unknown isolation {effective:?}")));
    }
    Ok((
        line.to_owned(),
        PathBuf::from(slot_path),
        effective.to_owned(),
    ))
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            workspace_id: None,
            profile: None,
            harness: "codex".to_owned(),
            model: None,
            resume: None,
            harness_args: Vec::new(),
            environment: Vec::new(),
            brief: None,
            startup_timeout: Duration::from_secs(30),
            delivery: DrainOptions::default(),
            cloud: None,
            slot: None,
        }
    }
}

/// Required live-identity assertions for a pre-existing Herdr agent.
#[derive(Clone, Debug)]
pub struct AdoptOptions {
    /// Exact live pane containing the harness.
    pub pane_id: String,
    /// Expected live workspace label.
    pub expected_workspace: String,
    /// Expected live harness working directory.
    pub cwd: PathBuf,
    /// Expected Herdr harness kind.
    pub harness: String,
    /// Optional native session ID already reported by the exact pane.
    pub session: Option<String>,
}

/// Explicit safety assertions for stop/recovery.
#[derive(Clone, Debug, Default)]
pub struct StopOptions {
    /// Exact current registry generation token.
    pub expected_token: Option<String>,
    /// Enable the narrowly scoped identity-less adopted-row recovery path.
    pub recover_legacy_adoption: bool,
    /// Retire a dead adopted harness's record without inspecting or changing its runtime.
    pub retire_dead_adoption: bool,
    /// SHA-256 of the exact current `agent.json` bytes for explicit adoption recovery.
    pub expected_record_sha256: Option<String>,
    /// Retire an agentcloud agent's record and tab without halting or archiving its session.
    pub skip_cloud_halt: bool,
}

/// Registry-backed coordinator interface to visible foreign-harness workers.
pub struct ManagedAgents<'a, A: ManagedApi + ?Sized> {
    client: &'a A,
    registry: PathBuf,
    /// Test-only project workspace override. Production policy is loaded lazily
    /// for each operation that needs it, so long-running managers do not cache it.
    project_workspace_override: Option<String>,
    /// Workspace inherited from the launching Herdr pane (`HERDR_WORKSPACE_ID`),
    /// used when a start names none.
    inherited_workspace: Option<String>,
    /// Executables for agentcloud-backed agents.
    cloud_tools: CloudTools,
    #[cfg(test)]
    revive_wrkslots_executable: Option<PathBuf>,
}

impl<'a, A: ManagedApi + ?Sized> ManagedAgents<'a, A> {
    /// Use an explicit registry, independent of the agents' working directories.
    pub fn new(client: &'a A, registry: &Path) -> Result<Self> {
        let registry = if registry.is_absolute() {
            registry.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| fail(error.to_string()))?
                .join(registry)
        };
        let inherited_workspace = std::env::var("HERDR_WORKSPACE_ID")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            client,
            registry,
            project_workspace_override: None,
            inherited_workspace,
            cloud_tools: CloudTools::default(),
            #[cfg(test)]
            revive_wrkslots_executable: None,
        })
    }

    /// Select the `agentcloudctl` and `agentterm` executables for agentcloud agents.
    #[must_use]
    pub fn with_cloud_tools(mut self, tools: CloudTools) -> Self {
        self.cloud_tools = tools;
        self
    }

    /// Replace the inherited workspace, so tests never depend on the pane that runs them.
    #[cfg(test)]
    fn with_inherited_workspace(mut self, workspace: Option<&str>) -> Self {
        self.inherited_workspace = workspace.map(str::to_owned);
        self
    }

    #[cfg(test)]
    fn with_project_workspace(mut self, workspace: Option<&str>) -> Self {
        self.project_workspace_override = workspace.map(str::to_owned);
        self
    }

    fn target(&self, record: &AgentRecord) -> Result<Target> {
        record.target()
    }

    fn project_workspace(&self) -> Result<Option<String>> {
        match self.project_workspace_override.as_ref() {
            Some(workspace) => Ok(Some(workspace.clone())),
            None => crate::profiles::workspace_for_registry(&self.registry),
        }
    }

    fn policy_target(&self, record: &AgentRecord) -> Result<Target> {
        let mut target = record.target()?;
        target.expected_workspace = self.project_workspace()?;
        Ok(target)
    }

    fn directory(&self, agent_name: &str) -> Result<PathBuf> {
        Ok(self.registry.join(name(agent_name)?))
    }

    fn lock(&self, agent_name: &str) -> Result<File> {
        self.lock_with_runtime(agent_name, &agent::SystemRuntime::default())
    }

    fn lock_with_runtime(
        &self,
        agent_name: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<File> {
        name(agent_name)?;
        agent::create_private_directory(&self.registry, "agent registry", true, true)?;
        let path = self.registry.join(format!(".{agent_name}.lock"));
        let file = agent::open_private_lock(&path, "agent lifecycle lock")?;
        agent::lock_exclusive_with_runtime(&file, &path, "agent lifecycle", runtime)?;
        Ok(file)
    }

    fn identity_lock(&self) -> Result<File> {
        agent::create_private_directory(&self.registry, "agent registry", true, true)?;
        let file =
            agent::open_private_lock(&self.registry.join(".identity.lock"), "agent identity lock")?;
        file.lock_exclusive()
            .map_err(|error| fail(error.to_string()))?;
        Ok(file)
    }

    fn pane_lock(&self, pane_id: &str) -> Result<agent::TargetLock> {
        agent::lock_target(pane_id, "host-wide target lock")
    }

    fn private_directory_identity(metadata: &fs::Metadata, label: &str) -> Result<(u64, u64)> {
        let uid = unsafe { libc::getuid() };
        if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(fail(format!("{label} is not a private directory")));
        }
        Ok((metadata.dev(), metadata.ino()))
    }

    fn pinned_agent_directory(&self, agent_name: &str) -> Result<PinnedAgentDirectory> {
        self.pinned_agent_directory_with(agent_name, || {})
    }

    fn pinned_agent_directory_with(
        &self,
        agent_name: &str,
        after_preopen_check: impl FnOnce(),
    ) -> Result<PinnedAgentDirectory> {
        let path = self.directory(agent_name)?;
        agent::validate_private_directory(&path, "agent directory", false)?;
        let before = fs::symlink_metadata(&path)
            .map_err(|error| fail(format!("cannot inspect agent directory: {error}")))?;
        let (device, inode) = Self::private_directory_identity(&before, "agent directory")?;
        after_preopen_check();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| fail(format!("cannot pin agent directory: {error}")))?;
        let metadata = file
            .metadata()
            .map_err(|error| fail(format!("cannot inspect pinned agent directory: {error}")))?;
        let opened = Self::private_directory_identity(&metadata, "pinned agent directory")?;
        if opened != (device, inode) {
            return Err(fail("agent directory changed while being pinned"));
        }
        let pinned = PinnedAgentDirectory {
            name: agent_name.to_owned(),
            path,
            file,
            device,
            inode,
        };
        Self::verify_pinned_agent_directory(&pinned)?;
        Ok(pinned)
    }

    fn verify_pinned_agent_directory(pinned: &PinnedAgentDirectory) -> Result<()> {
        let path = fs::symlink_metadata(&pinned.path)
            .map_err(|error| fail(format!("agent registry directory changed: {error}")))?;
        let opened = pinned
            .file
            .metadata()
            .map_err(|error| fail(format!("cannot reinspect pinned agent directory: {error}")))?;
        let path_identity = Self::private_directory_identity(&path, "agent directory")?;
        let opened_identity = Self::private_directory_identity(&opened, "pinned agent directory")?;
        if path_identity != (pinned.device, pinned.inode)
            || opened_identity != (pinned.device, pinned.inode)
        {
            return Err(fail(format!(
                "agent {:?} registry directory changed",
                pinned.name
            )));
        }
        Ok(())
    }

    fn pinned_parent_directory(path: &Path, label: &str) -> Result<PinnedParentDirectory> {
        Self::pinned_parent_directory_with(path, label, || {})
    }

    fn pinned_parent_directory_with(
        path: &Path,
        label: &str,
        after_preopen_check: impl FnOnce(),
    ) -> Result<PinnedParentDirectory> {
        agent::validate_private_directory(path, label, false)?;
        let before = fs::symlink_metadata(path)
            .map_err(|error| fail(format!("cannot inspect {label}: {error}")))?;
        let (device, inode) = Self::private_directory_identity(&before, label)?;
        after_preopen_check();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| fail(format!("cannot pin {label}: {error}")))?;
        let metadata = file
            .metadata()
            .map_err(|error| fail(format!("cannot inspect pinned {label}: {error}")))?;
        let opened = Self::private_directory_identity(&metadata, &format!("pinned {label}"))?;
        if opened != (device, inode) {
            return Err(fail(format!("{label} changed while being pinned")));
        }
        let pinned = PinnedParentDirectory {
            path: path.to_owned(),
            file,
            device,
            inode,
        };
        Self::verify_pinned_parent_directory(&pinned, label)?;
        Ok(pinned)
    }

    fn verify_pinned_parent_directory(pinned: &PinnedParentDirectory, label: &str) -> Result<()> {
        let path = fs::symlink_metadata(&pinned.path)
            .map_err(|error| fail(format!("{label} changed: {error}")))?;
        let opened = pinned
            .file
            .metadata()
            .map_err(|error| fail(format!("cannot reinspect pinned {label}: {error}")))?;
        let path_identity = Self::private_directory_identity(&path, label)?;
        let opened_identity =
            Self::private_directory_identity(&opened, &format!("pinned {label}"))?;
        if path_identity != (pinned.device, pinned.inode)
            || opened_identity != (pinned.device, pinned.inode)
        {
            return Err(fail(format!("{label} changed")));
        }
        Ok(())
    }

    fn child_directory_identity(parent: &File, name: &str) -> Result<Option<(u64, u64)>> {
        let name = CString::new(name).map_err(|_| fail("registry entry name contains NUL"))?;
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_RDONLY,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(fail(format!(
                "cannot inspect registry directory entry: {error}"
            )));
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file
            .metadata()
            .map_err(|error| fail(format!("cannot inspect registry directory entry: {error}")))?;
        Ok(Some(Self::private_directory_identity(
            &metadata,
            "registry directory entry",
        )?))
    }

    fn open_pinned_file(
        pinned: &PinnedAgentDirectory,
        name: &str,
        flags: libc::c_int,
    ) -> Result<File> {
        let name = CString::new(name).map_err(|_| fail("agent record name contains NUL"))?;
        let descriptor = unsafe {
            libc::openat(
                pinned.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            return Err(fail(format!(
                "cannot open pinned agent file: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn open_optional_pinned_file(
        pinned: &PinnedAgentDirectory,
        name: &str,
        flags: libc::c_int,
    ) -> Result<Option<File>> {
        let name = CString::new(name).map_err(|_| fail("agent record name contains NUL"))?;
        let descriptor = unsafe {
            libc::openat(
                pinned.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if descriptor >= 0 {
            return Ok(Some(unsafe { File::from_raw_fd(descriptor) }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        Err(fail(format!("cannot open pinned agent file: {error}")))
    }

    fn record_bytes(&self, pinned: &PinnedAgentDirectory) -> Result<Vec<u8>> {
        self.record_bytes_with(pinned, true)
    }

    fn record_bytes_with(
        &self,
        pinned: &PinnedAgentDirectory,
        require_active_name: bool,
    ) -> Result<Vec<u8>> {
        self.record_bytes_with_identity(pinned, require_active_name)
            .map(|(content, _identity)| content)
    }

    fn record_bytes_with_identity(
        &self,
        pinned: &PinnedAgentDirectory,
        require_active_name: bool,
    ) -> Result<(Vec<u8>, (u64, u64))> {
        if require_active_name {
            Self::verify_pinned_agent_directory(pinned)?;
        }
        let path = pinned.path.join("agent.json");
        let mut file = Self::open_pinned_file(pinned, "agent.json", libc::O_RDONLY)?;
        let before = file.metadata().map_err(|error| {
            fail(format!(
                "cannot inspect agent record {}: {error}",
                path.display()
            ))
        })?;
        let uid = unsafe { libc::getuid() };
        if !before.is_file()
            || before.uid() != uid
            || before.permissions().mode() & 0o077 != 0
            || before.nlink() != 1
            || before.len() > MAX_AGENT_RECORD_BYTES as u64
        {
            return Err(fail(format!(
                "unsafe agent record for recovery: {}",
                path.display()
            )));
        }
        let mut content = Vec::with_capacity(before.len() as usize);
        Read::by_ref(&mut file)
            .take((MAX_AGENT_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut content)
            .map_err(|error| {
                fail(format!(
                    "cannot read agent record {}: {error}",
                    path.display()
                ))
            })?;
        let after = file.metadata().map_err(|error| {
            fail(format!(
                "cannot reinspect agent record {}: {error}",
                path.display()
            ))
        })?;
        if content.len() > MAX_AGENT_RECORD_BYTES
            || before.len() != content.len() as u64
            || after.dev() != before.dev()
            || after.ino() != before.ino()
            || after.mode() != before.mode()
            || after.uid() != before.uid()
            || after.nlink() != before.nlink()
            || after.len() != before.len()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
        {
            return Err(fail(format!(
                "agent record changed while reading: {}",
                path.display()
            )));
        }
        if require_active_name {
            Self::verify_pinned_agent_directory(pinned)?;
        }
        let current = Self::open_pinned_file(pinned, "agent.json", libc::O_RDONLY)?;
        let current = current
            .metadata()
            .map_err(|error| fail(format!("cannot reinspect current agent record: {error}")))?;
        if (current.dev(), current.ino()) != (before.dev(), before.ino()) {
            return Err(fail("agent record pathname changed while reading"));
        }
        Ok((content, (before.dev(), before.ino())))
    }

    fn load(&self, agent_name: &str) -> Result<AgentRecord> {
        self.load_with_stop_advice(agent_name, None)
    }

    fn load_with_stop_advice(
        &self,
        agent_name: &str,
        recovery: Option<&mut RecoveryAction>,
    ) -> Result<AgentRecord> {
        self.directory(agent_name)?;
        agent::validate_private_directory(&self.registry, "agent registry", false)?;
        self.refuse_pending_rename_with_stop_advice(&[agent_name], recovery)?;
        self.read_record(agent_name)
    }

    /// Read and validate one record without consulting rename journals.
    fn read_record(&self, agent_name: &str) -> Result<AgentRecord> {
        let directory = self.directory(agent_name)?;
        if !directory.exists() {
            return Err(fail(format!(
                "unknown agent {agent_name:?}; use list to inspect the registry"
            )));
        }
        agent::validate_private_directory(&directory, "agent directory", false)?;
        let path = directory.join("agent.json");
        let document = agent::read_private_json(&path)?;
        let record = match nested::decode(&document, &path) {
            Some(record) => record?,
            None => serde_json::from_value(document).map_err(|error| {
                fail(format!("invalid agent record {}: {error}", path.display()))
            })?,
        };
        record.validate_loaded(&path, agent_name)?;
        Ok(record)
    }

    fn legacy_record_snapshot(
        &self,
        pinned: &PinnedAgentDirectory,
        expected_token: &str,
        expected_digest: &str,
    ) -> Result<LegacyRecordSnapshot> {
        if expected_digest.len() != 64
            || !expected_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(fail(
                "expected-record-sha256 must be exactly 64 lowercase hexadecimal characters",
            ));
        }
        let path = pinned.path.join("agent.json");
        let content = self.record_bytes(pinned)?;
        let document: Value = serde_json::from_slice(&content).map_err(|error| {
            fail(format!(
                "cannot inspect legacy agent record {}: {error}",
                path.display()
            ))
        })?;
        let object = document
            .as_object()
            .ok_or_else(|| fail(format!("invalid legacy agent record: {}", path.display())))?;
        if object.contains_key("foreign_shell_identity") {
            return Err(fail(
                "--recover-legacy-adoption requires foreign_shell_identity to be absent, not null or populated",
            ));
        }
        let record: AgentRecord = serde_json::from_value(document)
            .map_err(|error| fail(format!("invalid agent record {}: {error}", path.display())))?;
        record.validate_loaded(&path, &pinned.name)?;
        let digest = format!("{:x}", Sha256::digest(&content));
        if record.token != expected_token || digest != expected_digest {
            return Err(fail(format!(
                "agent {:?} record changed before adoption recovery",
                pinned.name
            )));
        }
        Ok(LegacyRecordSnapshot {
            record,
            content,
            digest,
            directory_device: pinned.device,
            directory_inode: pinned.inode,
        })
    }

    fn managed_record_snapshot(
        &self,
        pinned: &PinnedAgentDirectory,
        expected_token: &str,
    ) -> Result<ManagedRecordSnapshot> {
        let path = pinned.path.join("agent.json");
        let content = self.record_bytes(pinned)?;
        let record = match nested::decode_bytes(&content, &path) {
            Some(record) => record?,
            None => serde_json::from_slice(&content).map_err(|error| {
                fail(format!("invalid agent record {}: {error}", path.display()))
            })?,
        };
        record.validate_loaded(&path, &pinned.name)?;
        if record.token != expected_token {
            return Err(fail(format!(
                "agent {:?} was replaced before this operation",
                pinned.name
            )));
        }
        Ok(ManagedRecordSnapshot {
            record,
            content,
            directory_device: pinned.device,
            directory_inode: pinned.inode,
        })
    }

    fn dead_adoption_snapshot(
        &self,
        pinned: &PinnedAgentDirectory,
        expected_token: &str,
    ) -> Result<DeadAdoptionSnapshot> {
        let (content, record_identity) = self.record_bytes_with_identity(pinned, true)?;
        let path = pinned.path.join("agent.json");
        let record = match nested::decode_bytes(&content, &path) {
            Some(record) => record?,
            None => serde_json::from_slice(&content).map_err(|error| {
                fail(format!("invalid agent record {}: {error}", path.display()))
            })?,
        };
        record.validate_loaded(&path, &pinned.name)?;
        if record.token != expected_token {
            return Err(fail(format!(
                "agent {:?} was replaced before adoption retirement",
                pinned.name
            )));
        }
        Ok(DeadAdoptionSnapshot {
            record,
            content,
            record_identity,
            directory_identity: (pinned.device, pinned.inode),
        })
    }

    fn dead_adoption_anchor(record: &AgentRecord) -> Result<&CustomProcessIdentity> {
        if record.lifecycle != "running"
            || record.mode != "interactive"
            || record.backend != "herdr"
            || record.adapter != "herdr-foreign"
        {
            return Err(fail(
                "--retire-dead-adoption applies only to a running interactive herdr-foreign record",
            ));
        }
        record
            .harness_anchor()
            .filter(|identity| identity.valid())
            .ok_or_else(|| {
                fail("dead adoption retirement requires a valid harness anchor from the current pinning rule")
            })
    }

    fn save(&self, record: &AgentRecord) -> Result<()> {
        if let Some(storage) = record.storage_directory.as_ref() {
            let pinned = revive::pin_directory(&storage.path, &record.name)?;
            if (pinned.device, pinned.inode) != (storage.device, storage.inode) {
                return Err(fail("staged revive directory changed before save"));
            }
            let content = revive::record_document_bytes(record)?;
            atomic_replace_bytes(&pinned, "agent.json", &content).map_err(|error| *error.error)?;
            Self::verify_pinned_agent_directory(&pinned)?;
            return Ok(());
        }
        agent::atomic_json(
            &self.directory(&record.name)?.join("agent.json"),
            &record.document()?,
        )
    }

    fn queue(&self, agent_name: &str) -> Result<PathBuf> {
        Ok(self.directory(agent_name)?.join("queue"))
    }

    fn move_intent_path(&self, agent_name: &str) -> Result<PathBuf> {
        Ok(self.directory(agent_name)?.join("move.json"))
    }

    fn move_intent(&self, record: &AgentRecord, destination: &str) -> Value {
        json!({
            "schema": "agentctl-move/v1",
            "token": record.token,
            "source_pane_id": record.pane_id,
            "source_tab_id": record.tab_id,
            "source_workspace_id": record.workspace_id,
            "destination_workspace_id": destination,
            "harness": record.harness,
            "cwd": record.cwd,
            "session_agent": record.session_agent,
            "session_value": record.session_value,
        })
    }

    fn write_move_intent(&self, record: &AgentRecord, destination: &str) -> Result<()> {
        agent::atomic_json(
            &self.move_intent_path(&record.name)?,
            &self.move_intent(record, destination),
        )
    }

    fn require_move_intent(&self, record: &AgentRecord, destination: &str) -> Result<()> {
        let Some(actual) = self.read_move_intent(record)? else {
            return Err(fail(format!(
                "refusing to recover move of {:?}: no durable move intent",
                record.name
            )));
        };
        if actual != self.move_intent(record, destination) {
            return Err(fail(format!(
                "refusing to recover move of {:?}: durable move intent changed; rerun `agentctl move {}`",
                record.name, record.name
            )));
        }
        Ok(())
    }

    fn read_move_intent(&self, record: &AgentRecord) -> Result<Option<Value>> {
        self.read_move_intent_with_stop_context(record, false)
    }

    fn read_move_intent_with_stop_context(
        &self,
        record: &AgentRecord,
        for_stop: bool,
    ) -> Result<Option<Value>> {
        let path = self.move_intent_path(&record.name)?;
        let unreadable = |error: String| {
            if for_stop {
                fail(format!(
                    "move of {:?} has an unreadable durable intent: {error}",
                    record.name
                ))
            } else {
                fail(format!(
                    "move of {:?} has an unreadable durable intent; rerun `agentctl move {}`: {error}",
                    record.name, record.name
                ))
            }
        };
        let actual = match fs::symlink_metadata(&path) {
            Ok(_) => {
                agent::read_private_json(&path).map_err(|error| unreadable(error.to_string()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unreadable(error.to_string())),
        };
        let valid = actual["schema"] == "agentctl-move/v1"
            && actual["token"] == record.token
            && actual["harness"] == record.harness
            && actual["cwd"] == record.cwd
            && actual["session_agent"] == json!(record.session_agent)
            && actual["session_value"] == json!(record.session_value)
            && [
                "source_pane_id",
                "source_tab_id",
                "source_workspace_id",
                "destination_workspace_id",
            ]
            .iter()
            .all(|key| actual[key].as_str().is_some());
        if !valid {
            if for_stop {
                return Err(fail(format!(
                    "move of {:?} has an invalid durable intent",
                    record.name
                )));
            }
            return Err(fail(format!(
                "move of {:?} has an invalid durable intent; rerun `agentctl move {}`",
                record.name, record.name
            )));
        }
        Ok(Some(actual))
    }

    fn pending_move_destination(&self, record: &AgentRecord) -> Result<Option<String>> {
        self.pending_move_destination_with_stop_context(record, false)
    }

    fn pending_move_destination_for_stop(&self, record: &AgentRecord) -> Result<Option<String>> {
        self.pending_move_destination_with_stop_context(record, true)
    }

    fn pending_move_destination_with_stop_context(
        &self,
        record: &AgentRecord,
        for_stop: bool,
    ) -> Result<Option<String>> {
        let Some(actual) = self.read_move_intent_with_stop_context(record, for_stop)? else {
            return Ok(None);
        };
        let destination = actual
            .get("destination_workspace_id")
            .and_then(Value::as_str)
            .expect("read_move_intent validated destination");
        let completed = record.workspace_id.as_deref() == Some(destination)
            && actual["source_workspace_id"].as_str() != Some(destination);
        if actual != self.move_intent(record, destination) && !completed {
            if for_stop {
                return Err(fail(format!(
                    "move of {:?} has a durable intent that does not match its record",
                    record.name
                )));
            }
            return Err(fail(format!(
                "move of {:?} has a durable intent that does not match its record; rerun `agentctl move {}`",
                record.name, record.name
            )));
        }
        Ok(Some(destination.to_owned()))
    }

    fn source_unchanged_after_failed_move(&self, record: &AgentRecord) -> Result<bool> {
        let Some(pane_id) = record.pane_id.as_deref() else {
            return Ok(false);
        };
        if self.client.agent_pane(&record.name)? != pane_id {
            return Ok(false);
        }
        let presentations: Vec<Pane> = self
            .client
            .panes()?
            .into_iter()
            .filter(|pane| pane.pane_id == pane_id)
            .collect();
        if presentations.len() != 1
            || Some(presentations[0].tab_id.as_str()) != record.tab_id.as_deref()
            || Some(presentations[0].workspace_id.as_str()) != record.workspace_id.as_deref()
        {
            return Ok(false);
        }
        let info = self.client.pane_info(pane_id)?;
        Ok(info.pane_id == pane_id
            && Some(info.workspace_id.as_str()) == record.workspace_id.as_deref()
            && fs::canonicalize(&info.cwd)
                .ok()
                .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok()))
    }

    fn clear_move_intent(&self, agent_name: &str) -> Result<()> {
        let path = self.move_intent_path(agent_name)?;
        match fs::remove_file(&path) {
            Ok(()) => agent::sync_directory(path.parent().expect("move intent has parent")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(fail(error.to_string())),
        }
    }

    /// A recipient checker for one record, with no queue or workspace policy.
    fn workspace_client<'b>(&'b self, record: &'b AgentRecord) -> WorkspaceClient<'b, A> {
        WorkspaceClient {
            client: self.client,
            record,
            goal_objective: Mutex::new(None),
            custom_submission: Mutex::new(None),
            queue: None,
            check_prompt: false,
            expected_workspace: None,
            readback: None,
        }
    }

    /// Pin the terminal and harness process of a pane this command just verified.
    ///
    /// Used only where agentctl itself launched the program, so the pinned identity is the
    /// intended recipient rather than whatever is visible now.
    fn anchor_fresh(&self, record: &mut AgentRecord, terminal_id: Option<String>) -> Result<()> {
        record.terminal_id = terminal_id;
        if matches!(record.adapter.as_str(), "herdr" | "herdr-foreign") {
            if let Some(pane) = record.pane_id.clone() {
                record.harness_identity = self.client.harness_identity(&pane, &record.harness)?;
                record.anchor_rule = record.harness_identity.as_ref().map(|_| ANCHOR_RULE);
            }
        }
        if let Some(pane) = record.pane_id.as_deref() {
            if let Some(owner) =
                self.claim_owner(pane, record.terminal_id.as_deref(), Some(&record.name))?
            {
                return Err(fail(format!(
                    "pane {pane} is already registered as '{}'",
                    owner
                )));
            }
        }
        Ok(())
    }

    /// After a verified move, keep the terminal anchor only while the harness still matches.
    ///
    /// A cross-workspace move may give the pane a new terminal id. The move itself proved the
    /// managed name followed the pane; the pinned harness process must also still be its
    /// foreground program before the new terminal is recorded.
    fn repin_moved_terminal(&self, record: &mut AgentRecord) -> Result<()> {
        let (Some(pane), Some(recorded)) = (record.pane_id.clone(), record.terminal_id.clone())
        else {
            return Ok(());
        };
        let terminal = self.client.pane_info(&pane)?.terminal_id;
        if terminal.as_deref() == Some(recorded.as_str()) {
            return Ok(());
        }
        let harness_matches = match record.harness_anchor() {
            Some(identity) => self.client.verify_harness_identity(&pane, identity)?,
            None => false,
        };
        record.terminal_id = if harness_matches { terminal } else { None };
        Ok(())
    }

    /// Return another active record that claims this pane or terminal.
    /// Name another active record that claims this pane or terminal.
    ///
    /// Read conservatively as plain JSON: an unreadable record refuses rather than being
    /// skipped, and a record this edition cannot fully decode still counts.
    fn claim_owner(
        &self,
        pane_id: &str,
        terminal_id: Option<&str>,
        exclude: Option<&str>,
    ) -> Result<Option<String>> {
        let claims = registry_claims(&self.registry, exclude.unwrap_or("")).map_err(fail)?;
        Ok(claims
            .into_iter()
            .find(|(_, pane, terminal)| {
                pane == pane_id
                    || terminal_id.is_some_and(|wanted| terminal.as_deref() == Some(wanted))
            })
            .map(|(name, _, _)| name))
    }

    fn renames_directory(&self) -> PathBuf {
        self.registry.join(".renames")
    }

    /// Read every pending rename journal; an unreadable one refuses.
    fn rename_journals(&self) -> Result<Vec<RenameJournal>> {
        let directory = self.renames_directory();
        if !directory.exists() {
            return Ok(Vec::new());
        }
        agent::validate_private_directory(&directory, "rename journal directory", false)?;
        let mut paths = fs::read_dir(&directory)
            .map_err(|error| fail(error.to_string()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| fail(error.to_string()))?;
        paths.sort();
        let mut journals = Vec::new();
        for path in paths {
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if file_name.starts_with('.') {
                continue; // an atomic writer's temporary file
            }
            if !file_name.ends_with(".json") {
                return Err(fail(format!(
                    "unexpected entry in rename journal directory: {}",
                    path.display()
                )));
            }
            let value = agent::read_private_json(&path)?;
            journals.push(RenameJournal::parse(&value, &path)?);
        }
        Ok(journals)
    }

    /// Refuse any of `names` while a rename journal reserves it.
    fn refuse_pending_rename(&self, names: &[&str]) -> Result<()> {
        self.refuse_pending_rename_with_stop_advice(names, None)
    }

    fn refuse_pending_rename_with_stop_advice(
        &self,
        names: &[&str],
        recovery: Option<&mut RecoveryAction>,
    ) -> Result<()> {
        if let Some(journal) = self
            .rename_journals()?
            .into_iter()
            .find(|journal| journal.names(names))
        {
            let for_stop = recovery.is_some();
            if let Some(recovery) = recovery {
                *recovery = RecoveryAction::Rename {
                    old: journal.old.clone(),
                    new: journal.new.clone(),
                    token: journal.token.clone(),
                };
            }
            if for_stop {
                return Err(fail(format!(
                    "rename of '{}' to '{}' is incomplete",
                    journal.old, journal.new
                )));
            }
            return Err(fail(format!(
                "rename of '{}' to '{}' is incomplete; rerun `agentctl rename {} {}`",
                journal.old, journal.new, journal.old, journal.new
            )));
        }
        self.refuse_pending_revive_with_stop_advice(names, recovery)
    }

    fn write_rename_journal(&self, journal: &RenameJournal) -> Result<()> {
        let directory = self.renames_directory();
        if !directory.exists() {
            DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(|error| fail(error.to_string()))?;
            // Publish the new directory entry before any runtime change relies on it.
            agent::sync_directory(&self.registry)?;
        }
        agent::validate_private_directory(&directory, "rename journal directory", false)?;
        let path = directory.join(format!("{}.json", journal.token));
        if fs::symlink_metadata(&path).is_ok() {
            return Err(fail(format!(
                "a rename journal already exists: {}",
                path.display()
            )));
        }
        agent::atomic_json(&path, &journal.document())
    }

    fn remove_rename_journal(&self, journal: &RenameJournal) -> Result<()> {
        let directory = self.renames_directory();
        fs::remove_file(directory.join(format!("{}.json", journal.token)))
            .map_err(|error| fail(format!("cannot remove rename journal: {error}")))?;
        agent::sync_directory(&directory)
    }

    /// Hold one existing queue's delivery and binding locks, in drain's order.
    ///
    /// A queue is created by the first send, under the name lock the caller holds, so a
    /// missing queue has nothing to exclude; it is never created here.
    fn queue_locks(&self, directory_name: &str) -> Result<Vec<File>> {
        let queue = self.directory(directory_name)?.join("queue");
        if !queue.is_dir() {
            return Ok(Vec::new());
        }
        let mut held = Vec::with_capacity(2);
        for (lock_name, purpose) in [
            (".delivery.lock", "queue delivery lock"),
            (".binding.lock", "queue binding lock"),
        ] {
            let lock = agent::open_private_lock(&queue.join(lock_name), purpose)?;
            lock.lock_exclusive()
                .map_err(|error| fail(format!("cannot take {purpose}: {error}")))?;
            held.push(lock);
        }
        Ok(held)
    }

    fn retirement_lock_identity(file: &File) -> Result<(u64, u64)> {
        let metadata = file.metadata().map_err(|error| {
            fail(format!(
                "cannot inspect adoption retirement queue lock: {error}"
            ))
        })?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(fail(
                "adoption retirement queue lock must be a private regular file with one link",
            ));
        }
        Ok((metadata.dev(), metadata.ino()))
    }

    fn open_retirement_queue_lock(directory: &File, lock_name: &str, create: bool) -> Result<File> {
        let name = CString::new(lock_name).map_err(|_| fail("queue lock name contains NUL"))?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_RDWR
                    | if create { libc::O_CREAT } else { 0 },
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(fail(format!(
                "cannot open adoption retirement queue lock {lock_name}: {}",
                io::Error::last_os_error()
            )));
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        Self::retirement_lock_identity(&file)?;
        Ok(file)
    }

    fn retirement_queue_locks(&self, pinned: &PinnedAgentDirectory) -> Result<RetirementQueue> {
        Self::verify_pinned_agent_directory(pinned)?;
        let identity = Self::child_directory_identity(&pinned.file, "queue")?;
        let mut held = RetirementQueue {
            directory: None,
            locks: Vec::new(),
        };
        let Some(identity) = identity else {
            return Ok(held);
        };
        let queue =
            Self::pinned_parent_directory(&pinned.path.join("queue"), "adoption retirement queue")?;
        if (queue.device, queue.inode) != identity {
            return Err(fail("adoption retirement queue changed before locking"));
        }
        held.directory = Some(queue);
        for name in [".delivery.lock", ".binding.lock"] {
            let queue = held.directory.as_ref().expect("existing queue is pinned");
            let file = Self::open_retirement_queue_lock(&queue.file, name, true)?;
            file.lock_exclusive()
                .map_err(|error| fail(format!("cannot lock adoption retirement queue: {error}")))?;
            held.locks.push((name, file));
            self.verify_retirement_queue(pinned, &held)?;
        }
        Ok(held)
    }

    fn verify_retirement_queue(
        &self,
        pinned: &PinnedAgentDirectory,
        held: &RetirementQueue,
    ) -> Result<()> {
        Self::verify_pinned_agent_directory(pinned)?;
        let expected = held
            .directory
            .as_ref()
            .map(|directory| (directory.device, directory.inode));
        if Self::child_directory_identity(&pinned.file, "queue")? != expected {
            return Err(fail("adoption retirement queue generation changed"));
        }
        if let Some(queue) = held.directory.as_ref() {
            Self::verify_pinned_parent_directory(queue, "adoption retirement queue")?;
            for (name, file) in &held.locks {
                let current = Self::open_retirement_queue_lock(&queue.file, name, false)?;
                if Self::retirement_lock_identity(&current)?
                    != Self::retirement_lock_identity(file)?
                {
                    return Err(fail(format!(
                        "adoption retirement queue lock {name} changed"
                    )));
                }
            }
            Self::verify_pinned_parent_directory(queue, "adoption retirement queue")?;
        }
        Self::verify_pinned_agent_directory(pinned)
    }

    /// Pin the terminal and harness process an operator has confirmed for NAME.
    pub fn anchor(&self, agent_name: &str, replace: bool) -> Result<Value> {
        let _lock = self.lock(agent_name)?;
        let _identity_lock = self.identity_lock()?;
        let mut record = self.load(agent_name)?;
        // Anchors live only in flat schema-1 records, so a nested row is rewritten in that form.
        let migrated_from = record.migrate_nested_to_flat();
        if !matches!(record.adapter.as_str(), "herdr" | "herdr-foreign") {
            return Err(fail(
                "anchor applies to native and adopted Herdr agents; custom panes pin their process at launch",
            ));
        }
        let Some(pane) = record.pane_id.clone() else {
            return Err(fail(format!("agent '{agent_name}' has no confirmed pane")));
        };
        let _pane_lock = self.pane_lock(&pane)?;
        let info = self.checked(&record)?;
        let Some(harness) = self.client.harness_identity(&pane, &record.harness)? else {
            return Err(fail(format!(
                "cannot pin the foreground {} process of pane {pane}; is the harness running in the foreground?",
                record.harness
            )));
        };
        let changed = record
            .terminal_id
            .as_ref()
            .is_some_and(|terminal| Some(terminal) != info.terminal_id.as_ref())
            || record
                .harness_anchor()
                .is_some_and(|identity| *identity != harness);
        if changed && !replace {
            return Err(fail(format!(
                "agent '{agent_name}' is anchored to a different terminal or harness process, so the pane may hold another program; inspect it, then rerun with --replace"
            )));
        }
        if let Some(owner) =
            self.claim_owner(&pane, info.terminal_id.as_deref(), Some(agent_name))?
        {
            return Err(fail(format!(
                "pane {pane} is already registered as '{}'",
                owner
            )));
        }
        let previous = json!({
            "terminal_id": record.terminal_id,
            "harness_pid": record.harness_identity.as_ref().map(|identity| identity.pid),
        });
        record.terminal_id = info.terminal_id;
        record.harness_identity = Some(harness.clone());
        record.anchor_rule = Some(ANCHOR_RULE);
        self.save(&record)?;
        Ok(json!({
            "name": agent_name,
            "pane_id": pane,
            "terminal_id": record.terminal_id,
            "harness_pid": harness.pid,
            "replaced": changed,
            "previous": previous,
            "migrated_from": migrated_from,
        }))
    }

    /// Rename a live agent: registry entry, Herdr agent name and tab label together.
    pub fn rename(&self, old: &str, new: &str) -> Result<Value> {
        self.refuse_pending_revive(&[old, new])?;
        name(old)?;
        name(new)?;
        if old == new {
            return Err(fail("rename needs two different names"));
        }
        let (first, second) = if old < new { (old, new) } else { (new, old) };
        let _first = self.lock(first)?;
        let _second = self.lock(second)?;
        let _identity_lock = self.identity_lock()?;
        let pending: Vec<RenameJournal> = self
            .rename_journals()?
            .into_iter()
            .filter(|journal| journal.names(&[old, new]))
            .collect();
        if let Some(found) = pending.first() {
            if pending.len() != 1 || found.old != old || found.new != new {
                return Err(fail(format!(
                    "rename of '{}' to '{}' is incomplete; rerun exactly `agentctl rename {} {}`",
                    found.old, found.new, found.old, found.new
                )));
            }
            return self.finish_rename(found, true);
        }
        let record = self.load(old)?;
        record.refuse_nested_rename()?;
        if !matches!(record.adapter.as_str(), "herdr" | "herdr-foreign")
            || record.lifecycle != "running"
        {
            return Err(fail(
                "rename supports running native and adopted Herdr agents",
            ));
        }
        let (Some(pane), Some(tab)) = (record.pane_id.clone(), record.tab_id.clone()) else {
            return Err(fail(format!("agent '{old}' has no confirmed pane and tab")));
        };
        if record.harness_anchor().is_none() && record.session_value.is_none() {
            return Err(fail(format!(
                "agent '{old}' pins no harness process or observed session; check its pane, run `agentctl anchor {old}`, then rename"
            )));
        }
        if record.name_history.len() >= MAX_NAME_HISTORY {
            return Err(fail(format!(
                "agent '{old}' has been renamed {} times, the most a record keeps; start a new \
                 agent instead",
                record.name_history.len()
            )));
        }
        if self.read_move_intent(&record)?.is_some() {
            return Err(fail(format!(
                "move of '{old}' is incomplete; rerun `agentctl move {old}`"
            )));
        }
        if fs::symlink_metadata(self.directory(new)?).is_ok() {
            return Err(fail(format!("agent '{new}' is already registered")));
        }
        let journal = RenameJournal {
            token: record.token.clone(),
            old: old.to_owned(),
            new: new.to_owned(),
            adapter: record.adapter.clone(),
            pane_id: pane.clone(),
            tab_id: tab,
            terminal_id: record.terminal_id.clone(),
            workspace_id: record.workspace_id.clone(),
            journal_id: new_journal_id()?,
            started_at: unix_seconds(),
        };
        let _queue_locks = self.queue_locks(old)?;
        let _pane_lock = self.pane_lock(&pane)?;
        self.checked(&record)?;
        self.workspace_client(&record)
            .verify_recipient(&pane, true)?;
        if let Some(owner) = self.claim_owner(&pane, record.terminal_id.as_deref(), Some(old))? {
            return Err(fail(format!(
                "pane {pane} is also claimed by registered agent '{owner}'; run `agentctl doctor`"
            )));
        }
        if self.client.agent_names()?.contains_key(new) {
            return Err(fail(format!("a Herdr agent is already named '{new}'")));
        }
        if record.adapter == "herdr" {
            if let Some(workspace) = record.workspace_id.as_deref() {
                if self
                    .client
                    .tab_labels(workspace)?
                    .values()
                    .any(|label| label == new)
                {
                    return Err(fail(format!(
                        "a tab in the workspace is already labelled '{new}'"
                    )));
                }
            }
        }
        self.write_rename_journal(&journal)?;
        self.complete_rename(&journal, old, record, false)
    }

    /// Complete a journalled rename after a crash, refusing any state it did not create.
    fn finish_rename(&self, journal: &RenameJournal, recovered: bool) -> Result<Value> {
        let old_exists = fs::symlink_metadata(self.directory(&journal.old)?).is_ok();
        let new_exists = fs::symlink_metadata(self.directory(&journal.new)?).is_ok();
        if old_exists == new_exists {
            return Err(fail(format!(
                "rename journal for '{}' -> '{}' found {} agent directories; refusing to guess",
                journal.old,
                journal.new,
                if old_exists { "both" } else { "neither" }
            )));
        }
        let current = if old_exists {
            journal.old.as_str()
        } else {
            journal.new.as_str()
        };
        let directory = self.directory(current)?;
        agent::validate_private_directory(&directory, "agent directory", false)?;
        let path = directory.join("agent.json");
        let raw = agent::read_private_json(&path)?;
        let raw_name = raw.get("name").and_then(Value::as_str).unwrap_or_default();
        let allowed: &[&str] = if old_exists {
            &[journal.old.as_str(), journal.new.as_str()]
        } else {
            &[journal.new.as_str()]
        };
        if raw.get("token").and_then(Value::as_str) != Some(journal.token.as_str())
            || !allowed.contains(&raw_name)
        {
            return Err(fail(format!(
                "agent record {} does not match the rename journal; refusing to guess",
                path.display()
            )));
        }
        let record: AgentRecord = serde_json::from_value(raw.clone())
            .map_err(|error| fail(format!("invalid agent record {}: {error}", path.display())))?;
        record.validate_loaded(&path, raw_name)?;
        let _queue_locks = self.queue_locks(current)?;
        let _pane_lock = self.pane_lock(&journal.pane_id)?;
        self.complete_rename(journal, current, record, recovered)
    }

    /// Run the remaining rename steps; every step checks its own state first.
    fn complete_rename(
        &self,
        journal: &RenameJournal,
        current: &str,
        mut record: AgentRecord,
        recovered: bool,
    ) -> Result<Value> {
        let (old, new) = (journal.old.as_str(), journal.new.as_str());
        let (pane, tab) = (journal.pane_id.as_str(), journal.tab_id.as_str());
        let live = self
            .client
            .panes()?
            .iter()
            .any(|entry| entry.pane_id == pane);
        let mut herdr_steps = "skipped-pane-missing".to_owned();
        let mismatch = if live {
            self.verify_rename_recipient(&record, journal)?
        } else {
            None
        };
        if let Some(reason) = mismatch {
            // The harness exited or another program holds the pane: its name and label are
            // not ours to change, but the registry rename still completes.
            herdr_steps = format!("skipped-recipient-changed: {reason}");
        } else if live {
            if journal.adapter == "herdr" {
                let names = self.client.agent_names()?;
                if names.get(new).map(String::as_str) != Some(pane) {
                    if names.get(old).map(String::as_str) != Some(pane) {
                        return Err(fail(format!(
                            "pane {pane} is named neither '{old}' nor '{new}' in Herdr; refusing"
                        )));
                    }
                    self.client.rename_agent(pane, new)?;
                }
                let label = self.client.tab_label(tab)?;
                if label != new {
                    if label != old {
                        return Err(fail(format!(
                            "tab {tab} is labelled '{label}', neither '{old}' nor '{new}'; refusing"
                        )));
                    }
                    self.client.rename_tab(tab, new)?;
                }
            }
            herdr_steps = "done".to_owned();
        }
        let recorded_once = record
            .name_history
            .iter()
            .any(|entry| entry.journal_id == journal.journal_id);
        if current == old {
            record.name = new.to_owned();
            if !recorded_once {
                // A journal from before the history limit may find it full: keep the newest
                // names rather than publish a record no edition can read.
                let excess = (record.name_history.len() + 1).saturating_sub(MAX_NAME_HISTORY);
                let evicted: Vec<String> = record
                    .name_history
                    .drain(..excess)
                    .map(|entry| entry.name)
                    .collect();
                for former in evicted {
                    if !record.former_names.contains(&former) {
                        record.former_names.push(former);
                    }
                }
                record.name_history.push(NameHistoryEntry {
                    name: old.to_owned(),
                    renamed_at: journal.started_at,
                    journal_id: journal.journal_id.clone(),
                });
            }
            // The renamed content is published inside OLD first, so a crash before the move
            // leaves a record a rerun recognises by its token and journal id.
            agent::atomic_json(&self.directory(old)?.join("agent.json"), &json!(record))?;
            let registry = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(&self.registry)
                .map_err(|error| fail(format!("cannot open agent registry: {error}")))?;
            #[cfg(test)]
            if FAIL_RENAME_DIRECTORY_MOVE.with(|fail_once| fail_once.replace(false)) {
                return Err(fail("simulated crash before the directory move"));
            }
            rename_directory_noreplace_at(&registry, old, &registry, new).map_err(|error| {
                fail(format!(
                    "cannot move agent directory '{old}' to '{new}': {error}"
                ))
            })?;
            registry
                .sync_all()
                .map_err(|error| fail(format!("cannot sync agent registry: {error}")))?;
        } else if !recorded_once {
            return Err(fail(format!(
                "agent '{new}' was moved without its rename history; refusing to guess"
            )));
        }
        self.remove_rename_journal(journal)?;
        Ok(json!({
            "name": new,
            "previous_name": old,
            "token": record.token,
            "pane_id": pane,
            "recovered": recovered,
            "herdr_steps": herdr_steps,
            "external_references": format!(
                "wrkslots slots, chat bindings and scheduled prompts that name '{old}' are not changed by agentctl"
            ),
        }))
    }

    /// Every non-presentation anchor must hold; only the name and label may be OLD or NEW.
    /// Every non-presentation anchor must hold; only the name and label may be OLD or NEW.
    ///
    /// `Ok(Some(reason))` is a verified mismatch; an error is a check that could not be made.
    fn verify_rename_recipient(
        &self,
        record: &AgentRecord,
        journal: &RenameJournal,
    ) -> Result<Option<String>> {
        let pane = journal.pane_id.as_str();
        let info = self.client.pane_info(pane)?;
        let mut failures = Vec::new();
        if Some(&info.workspace_id) != record.workspace_id.as_ref() {
            failures.push(format!(
                "workspace is '{}', recorded {}",
                info.workspace_id,
                repr(record.workspace_id.as_deref())
            ));
        }
        let real = |path: &str| fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        if real(&info.cwd) != real(&record.cwd) {
            failures.push(format!("cwd is '{}', recorded '{}'", info.cwd, record.cwd));
        }
        if info.agent.as_deref() != Some(record.harness.as_str()) {
            failures.push(format!(
                "harness is {}, recorded '{}'",
                repr(info.agent.as_deref()),
                record.harness
            ));
        }
        if journal.terminal_id.is_some() && info.terminal_id != journal.terminal_id {
            failures.push(format!(
                "terminal is {}, recorded {}",
                repr(info.terminal_id.as_deref()),
                repr(journal.terminal_id.as_deref())
            ));
        }
        if info
            .tab_id
            .as_deref()
            .is_some_and(|tab| tab != journal.tab_id)
        {
            failures.push(format!(
                "tab is {}, recorded '{}'",
                repr(info.tab_id.as_deref()),
                journal.tab_id
            ));
        }
        if !session_matches(record, &info) {
            failures.push("the observed native session changed".to_owned());
        }
        if let Some(identity) = record.harness_anchor() {
            if !self.client.verify_harness_identity(pane, identity)? {
                failures
                    .push("the anchored harness process is no longer in the foreground".to_owned());
            }
        } else if record.session_value.is_none() {
            failures.push("no anchored harness process or observed session".to_owned());
        }
        Ok((!failures.is_empty()).then(|| {
            format!(
                "pane {pane} no longer holds this agent: {}",
                failures.join("; ")
            )
        }))
    }

    /// Compare every registry record with live Herdr state; read-only unless repairing labels.
    pub fn doctor(&self, repair_labels: bool) -> Result<Value> {
        let mut rows = Vec::new();
        let mut registry_findings = Vec::new();
        let journals = match self.rename_journals() {
            Ok(journals) => journals,
            Err(error) => {
                registry_findings.push(json!({
                    "finding": "rename-journal-unreadable",
                    "detail": error.to_string(),
                }));
                Vec::new()
            }
        };
        let journal_names: BTreeSet<&str> = journals
            .iter()
            .flat_map(|journal| [journal.old.as_str(), journal.new.as_str()])
            .collect();
        let mut records = Vec::new();
        if self.registry.exists() {
            let mut names = fs::read_dir(&self.registry)
                .map_err(|error| fail(error.to_string()))?
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| fail(error.to_string()))?;
            names.retain(|value| name(value).is_ok());
            names.sort();
            for agent_name in names {
                match self.read_record(&agent_name) {
                    Ok(record) => records.push(record),
                    Err(error) => rows.push(json!({
                        "name": agent_name,
                        "findings": [if journal_names.contains(agent_name.as_str()) {
                            "rename-incomplete"
                        } else {
                            "record-unreadable"
                        }],
                        "detail": error.to_string(),
                    })),
                }
            }
        }
        let panes: BTreeMap<String, Pane> = self
            .client
            .panes()?
            .into_iter()
            .map(|pane| (pane.pane_id.clone(), pane))
            .collect();
        let agent_names = self.client.agent_names()?;
        let workspaces: BTreeSet<&str> = records
            .iter()
            .filter_map(|record| record.workspace_id.as_deref())
            .filter(|workspace| !workspace.is_empty())
            .collect();
        let mut labels = BTreeMap::new();
        let mut missing_workspaces = BTreeSet::new();
        for workspace in workspaces {
            match self.client.tab_labels(workspace) {
                Ok(found) => labels.extend(found),
                Err(error) if missing_target(&error) == Some("workspace-missing") => {
                    missing_workspaces.insert(workspace);
                }
                Err(error) => return Err(error.into()),
            }
        }
        let mut claims: BTreeMap<&str, usize> = BTreeMap::new();
        for record in &records {
            for key in [record.pane_id.as_deref(), record.terminal_id.as_deref()]
                .into_iter()
                .flatten()
            {
                *claims.entry(key).or_default() += 1;
            }
        }
        for record in &records {
            let mut findings = if record
                .workspace_id
                .as_deref()
                .is_some_and(|workspace| missing_workspaces.contains(workspace))
            {
                vec!["workspace-missing"]
            } else {
                match self.doctor_findings(record, &panes, &agent_names, &labels) {
                    Ok(findings) => findings,
                    // Closed between the pane list and this record's checks.
                    Err(error) => vec![missing_target(&error).ok_or(error)?],
                }
            };
            if journal_names.contains(record.name.as_str()) {
                findings.push("rename-incomplete");
            }
            if [record.pane_id.as_deref(), record.terminal_id.as_deref()]
                .into_iter()
                .flatten()
                .any(|key| claims.get(key).copied().unwrap_or(0) > 1)
            {
                findings.push("duplicate-claim");
            }
            let mut row = json!({
                "name": record.name,
                "adapter": record.adapter,
                "pane_id": record.pane_id,
                "findings": findings,
            });
            if repair_labels
                && !findings.is_empty()
                && findings
                    .iter()
                    .all(|finding| matches!(*finding, "label-mismatch" | "herdr-name-mismatch"))
                && record.adapter == "herdr"
                && journals.is_empty()
            {
                row["repaired"] = json!(self.repair_presentation(&record.name)?);
            }
            rows.push(row);
        }
        let registered: BTreeSet<&str> =
            records.iter().map(|record| record.name.as_str()).collect();
        let mut workspace_findings = Vec::new();
        for (tab_id, label) in &labels {
            let owner = records
                .iter()
                .find(|record| record.tab_id.as_deref() == Some(tab_id.as_str()));
            let finding = match owner {
                Some(owner) if registered.contains(label.as_str()) && *label != owner.name => {
                    Some("label-collision")
                }
                Some(_) => None,
                None if registered.contains(label.as_str()) => Some("label-collision"),
                None => Some("unmanaged-tab"),
            };
            if let Some(finding) = finding {
                workspace_findings.push(json!({
                    "finding": finding,
                    "tab_id": tab_id,
                    "label": label,
                }));
            }
        }
        let clean = registry_findings.is_empty()
            && rows.iter().all(|row| {
                row["findings"].as_array().is_some_and(Vec::is_empty) || row["repaired"] == true
            })
            && workspace_findings
                .iter()
                .all(|item| item["finding"] == "unmanaged-tab");
        Ok(json!({
            "clean": clean,
            "records": rows,
            "registry": registry_findings,
            "workspace": workspace_findings,
            "journals": journals
                .iter()
                .map(|journal| json!({"old": journal.old, "new": journal.new}))
                .collect::<Vec<_>>(),
        }))
    }

    fn doctor_findings(
        &self,
        record: &AgentRecord,
        panes: &BTreeMap<String, Pane>,
        agent_names: &BTreeMap<String, String>,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<&'static str>> {
        let Some(pane) = record
            .pane_id
            .as_deref()
            .filter(|pane| panes.contains_key(*pane))
        else {
            return Ok(vec!["pane-missing"]);
        };
        if record.is_cloud() {
            // An agentcloud tab runs agentterm, not a Herdr-detected harness.
            return Ok(Vec::new());
        }
        let mut findings = Vec::new();
        let info = self.client.pane_info(pane)?;
        if Some(&info.workspace_id) != record.workspace_id.as_ref() {
            findings.push("workspace-mismatch");
        }
        if !same_directory(&info.cwd, &record.cwd) {
            findings.push("cwd-mismatch");
        }
        if record.terminal_id.is_some() && info.terminal_id != record.terminal_id {
            findings.push("terminal-mismatch");
        }
        if record.adapter != "herdr-foreign" {
            if let Some(tab) = record.tab_id.as_deref() {
                if panes[pane].tab_id != tab {
                    findings.push("tab-moved");
                }
                if labels.get(tab) != Some(&record.name) {
                    findings.push("label-mismatch");
                }
            }
        }
        if record.adapter == "herdr"
            && agent_names.get(&record.name).map(String::as_str) != Some(pane)
        {
            findings.push("herdr-name-mismatch");
        }
        if !session_matches(record, &info) {
            findings.push("session-mismatch");
        }
        match info.agent.as_deref() {
            None => findings.push("harness-exited"),
            Some(agent) if agent != record.harness => findings.push("harness-kind-mismatch"),
            Some(_) => {}
        }
        if let Some(identity) = record.harness_anchor() {
            if !self.client.verify_harness_identity(pane, identity)? {
                findings.push("harness-replaced");
            }
        } else if matches!(record.adapter.as_str(), "herdr" | "herdr-foreign")
            && record.observed_session_value().is_none()
        {
            findings.push("unanchored");
        }
        Ok(findings)
    }

    /// Restore one owned agent's label and Herdr name when every other anchor holds.
    fn repair_presentation(&self, agent_name: &str) -> Result<bool> {
        let _lock = self.lock(agent_name)?;
        let _identity_lock = self.identity_lock()?;
        let record = self.load(agent_name)?;
        let (Some(pane), Some(tab)) = (record.pane_id.clone(), record.tab_id.clone()) else {
            return Ok(false);
        };
        let _queue_locks = self.queue_locks(agent_name)?;
        let _pane_lock = self.pane_lock(&pane)?;
        let guard = self.workspace_client(&record);
        if guard.verify_recipient(&pane, false).is_err()
            || self
                .claim_owner(&pane, record.terminal_id.as_deref(), Some(agent_name))?
                .is_some()
            || self.client.pane_info(&pane)?.agent.as_deref() != Some(record.harness.as_str())
        {
            return Ok(false);
        }
        if let Some(workspace) = record.workspace_id.as_deref() {
            if self
                .client
                .tab_labels(workspace)?
                .iter()
                .any(|(other, label)| label == agent_name && *other != tab)
            {
                return Ok(false);
            }
        }
        let names = self.client.agent_names()?;
        match names.get(agent_name) {
            Some(owner) if *owner != pane => return Ok(false),
            Some(_) => {}
            None => self.client.rename_agent(&pane, agent_name)?,
        }
        if self.client.tab_label(&tab)? != agent_name {
            self.client.rename_tab(&tab, agent_name)?;
        }
        guard.verify_recipient(&pane, true)?;
        Ok(true)
    }

    /// Read durable metadata even if Herdr is unreachable.
    pub fn get(&self, agent_name: &str) -> Result<Value> {
        Ok(json!(self.load(agent_name)?))
    }

    fn identity_owner(
        &self,
        session_agent: &str,
        session_value: &str,
        exclude: Option<&str>,
    ) -> Result<Option<AgentRecord>> {
        if !self.registry.exists() {
            return Ok(None);
        }
        agent::validate_private_directory(&self.registry, "agent registry", false)?;
        for entry in fs::read_dir(&self.registry).map_err(|error| fail(error.to_string()))? {
            let entry = entry.map_err(|error| fail(error.to_string()))?;
            let existing_name = entry.file_name().to_string_lossy().into_owned();
            if name(&existing_name).is_err() || Some(existing_name.as_str()) == exclude {
                continue;
            }
            let other = self.read_record(&existing_name)?;
            if (other.session_agent.as_deref().unwrap_or(&other.harness) == session_agent
                && (other.session_value.as_deref() == Some(session_value)
                    || other.goal_session_id.as_deref() == Some(session_value)
                    || other.resume.as_deref() == Some(session_value)))
                || other.native_session.as_ref().is_some_and(|native| {
                    native.agent == session_agent && native.value == session_value
                })
            {
                return Ok(Some(other));
            }
        }
        for other in revive::pending_claim_records(&self.registry)? {
            if Some(other.name.as_str()) != exclude
                && ((other.session_agent.as_deref().unwrap_or(&other.harness) == session_agent
                    && (other.session_value.as_deref() == Some(session_value)
                        || other.goal_session_id.as_deref() == Some(session_value)
                        || other.resume.as_deref() == Some(session_value)))
                    || other.native_session.as_ref().is_some_and(|native| {
                        native.agent == session_agent && native.value == session_value
                    }))
            {
                return Ok(Some(other));
            }
        }
        Ok(None)
    }

    /// Start one fresh tab and retain failed launch artifacts for diagnosis.
    pub fn start(&self, agent_name: &str, cwd: &Path, options: StartOptions) -> Result<Value> {
        self.start_inner(agent_name, cwd, options, None)
    }

    /// Start one fresh tab with an explicit structured reasoning effort.
    pub fn start_with_reasoning_effort(
        &self,
        agent_name: &str,
        cwd: &Path,
        reasoning_effort: &str,
        options: StartOptions,
    ) -> Result<Value> {
        self.start_inner(agent_name, cwd, options, Some(reasoning_effort))
    }

    /// Load the project workspace policy and check an explicit workspace against it.
    fn start_workspace_policy(&self, options: &StartOptions) -> Result<Option<String>> {
        let project_workspace = self.project_workspace()?;
        if let (Some(expected), Some(workspace)) = (
            project_workspace.as_deref(),
            options.workspace_id.as_deref(),
        ) {
            let actual_label = self.client.workspace_label(workspace)?;
            if actual_label != expected {
                return Err(fail(format!(
                    "workspace {workspace:?} is labelled {actual_label:?}, but project configuration requires {expected:?}"
                )));
            }
        }
        Ok(project_workspace)
    }

    fn start_inner(
        &self,
        agent_name: &str,
        cwd: &Path,
        mut options: StartOptions,
        reasoning_effort: Option<&str>,
    ) -> Result<Value> {
        name(agent_name)?;
        let cwd = fs::canonicalize(cwd)
            .map_err(|_| fail(format!("cwd is not a directory: {}", cwd.display())))?;
        if !cwd.is_dir() {
            return Err(fail(format!("cwd is not a directory: {}", cwd.display())));
        }
        if options.startup_timeout.is_zero() || options.startup_timeout > Duration::from_secs(300) {
            return Err(fail("startup timeout must be between 0 and 300 seconds"));
        }
        if options.brief.as_deref().is_some_and(str::is_empty) {
            return Err(fail("brief must not be empty"));
        }
        if options
            .profile
            .as_deref()
            .is_some_and(|value| !name_pattern(value))
        {
            return Err(fail("profile must match [a-z][a-z0-9-]{0,31}"));
        }
        if options.harness == cloud::CLOUD_HARNESS {
            if options.slot.is_some() {
                return Err(fail("--slot does not apply to harness agentcloud"));
            }
            return self.start_cloud(agent_name, cwd, options, reasoning_effort);
        }
        if options.cloud.is_some() {
            return Err(fail(
                "agentcloud settings apply only with harness agentcloud (--harness agentcloud)",
            ));
        }
        if options.harness == "claude"
            && options
                .harness_args
                .iter()
                .any(|argument| claude_conversation_selector(argument))
        {
            return Err(fail(
                "Claude conversation selectors must use --resume; agentctl assigns --session-id for new conversations",
            ));
        }
        if options
            .resume
            .as_deref()
            .is_some_and(|value| !valid_metadata_text(value))
        {
            return Err(fail(
                "native session id must be nonempty and contain no NUL",
            ));
        }
        if matches!(options.harness.as_str(), "codex" | "claude" | "muse") {
            crate::profiles::validate_structured_harness_argument_conflicts(
                &format!("{} launch", options.harness),
                &options.harness,
                &options.harness_args,
                options.model.is_some(),
                reasoning_effort.is_some(),
                options.resume.is_some(),
            )?;
        }
        let mut structured_arguments =
            crate::profiles::reasoning_arguments(&options.harness, reasoning_effort)?;
        structured_arguments.extend_from_slice(&options.harness_args);
        let native_session = match options.resume.as_deref() {
            Some(value) => Some(NativeSession::new(&options.harness, value, "asserted")),
            None if options.harness == "claude" => Some(NativeSession::new(
                "claude",
                &new_conversation_id()?,
                "asserted",
            )),
            None => None,
        };
        let mut arguments = harness_arguments(
            &options.harness,
            options.model.as_deref(),
            options.resume.as_deref(),
            &structured_arguments,
        )?;
        if options.harness == "claude" && options.resume.is_none() {
            let mut assigned_arguments = vec![
                "--session-id".to_owned(),
                native_session
                    .as_ref()
                    .expect("assigned Claude session")
                    .value
                    .clone(),
            ];
            assigned_arguments.extend(arguments);
            arguments = assigned_arguments;
        }
        options.environment = environment_entries(&options.environment)?;
        // Placement policy is an input/start concern. Load it before creating
        // the generation so a configuration error cannot leave the name taken.
        let project_workspace = self.start_workspace_policy(&options)?;
        // The agent works in the slot: its record and pane cwd are the slot
        // directory, which is where the boxed shell starts.
        let mut relay_command = None;
        let mut slot_project = None;
        let mut slot_isolation = None;
        let mut launch_extra = BTreeMap::new();
        let (cwd, slot_command) = match options.slot.as_ref() {
            Some(launch) => {
                if !valid_metadata_text(&launch.slot) {
                    return Err(fail("slot must be nonempty and contain no NUL"));
                }
                let project = launch.project.clone().unwrap_or_else(|| cwd.clone());
                let project = fs::canonicalize(&project).map_err(|_| {
                    fail(format!(
                        "slot project is not a directory: {}",
                        project.display()
                    ))
                })?;
                let mut launch = launch.clone();
                if launch.project_box && launch.box_cwd.is_none() {
                    // A coordinator box starts where the agent was asked to work.
                    launch.box_cwd = Some(cwd.clone());
                }
                let launch = &launch;
                let (line, slot_path, isolation) = slot_shell_command(launch, &project, &[])?;
                if launch.project_box {
                    launch_extra.insert(
                        "project_box".to_owned(),
                        json!({
                            "name": launch.slot,
                            "project": project,
                            "isolation": isolation,
                            "writable": launch.box_writable,
                            "cwd": launch.box_cwd,
                        }),
                    );
                } else {
                    slot_project = Some(project.display().to_string());
                    slot_isolation = Some(isolation.clone());
                }
                let slot_path = fs::canonicalize(&slot_path).map_err(|_| {
                    fail(format!(
                        "slot directory is not a directory: {}",
                        slot_path.display()
                    ))
                })?;
                if isolation == "root" {
                    // sudo stays the pane's foreground process under root isolation, so Herdr
                    // cannot start the harness; the boxed harness line runs in the pane
                    // directly and agentctl owns its lifecycle state.
                    if !RELAY_HARNESSES.contains(&options.harness.as_str()) {
                        return Err(fail(format!(
                            "{} with root isolation supports {}, not {:?}",
                            if launch.project_box {
                                "--project-box"
                            } else {
                                "--slot"
                            },
                            RELAY_HARNESSES.join(", "),
                            options.harness
                        )));
                    }
                    let executable = self.client.harness_executable(&options.harness)?;
                    let mut argv = vec![executable.display().to_string()];
                    argv.extend(arguments.iter().cloned());
                    let (relay_line, _path, _isolation) =
                        slot_shell_command(launch, &project, &argv)?;
                    relay_command = Some(relay_line);
                    (slot_path, None)
                } else {
                    (slot_path, Some(line))
                }
            }
            None => (cwd, None),
        };
        let _lock = self.lock(agent_name)?;
        let identity_lock = self.identity_lock()?;
        if let Some(resume) = options.resume.as_deref() {
            if let Some(owner) = self.identity_owner(&options.harness, resume, Some(agent_name))? {
                return Err(fail(format!(
                    "native session is already registered as {:?}",
                    owner.name
                )));
            }
        }
        let directory = self.directory(agent_name)?;
        self.refuse_pending_rename(&[agent_name])?;
        if fs::symlink_metadata(&directory).is_ok() {
            return Err(fail(format!(
                "agent {agent_name:?} already registered; stop it before reusing the name"
            )));
        }
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&self.registry)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?;
        let mut record = AgentRecord {
            storage_directory: None,
            nested: None,
            adapter: if relay_command.is_some() {
                "herdr-relay".to_owned()
            } else if options.harness == "muse" {
                "herdr-pane".to_owned()
            } else {
                herdr_adapter()
            },
            mode: interactive_mode(),
            backend: herdr_adapter(),
            paused: false,
            runtime_home: None,
            pane_reported_by_agentctl: false,
            custom_process_identity: None,
            foreign_shell_identity: None,
            terminal_id: None,
            harness_identity: None,
            name_history: Vec::new(),
            anchor_rule: None,
            former_names: Vec::new(),
            native_session,
            profile: options.profile.clone(),
            slot: options
                .slot
                .as_ref()
                .filter(|launch| !launch.project_box)
                .map(|launch| launch.slot.clone()),
            slot_project,
            slot_isolation,
            reasoning_effort: reasoning_effort.map(str::to_owned),
            environment_names: environment_names(&options.environment),
            agentcloud: None,
            extra: launch_extra,
            name: agent_name.to_owned(),
            token: format!("{}-{}", now.as_nanos(), std::process::id()),
            harness: options.harness.clone(),
            cwd: cwd.display().to_string(),
            created_at: now.as_secs_f64(),
            schema: 1,
            lifecycle: "starting".to_owned(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            session_agent: None,
            session_value: None,
            model: options.model.clone(),
            resume: options.resume.clone(),
            arguments,
            startup_warning: None,
            effective_reasoning_effort: None,
            error: None,
            goal: None,
            goal_delivery: None,
            goal_session_id: None,
            goal_command: None,
            goal_messages: BTreeMap::new(),
            goal_message_id: None,
        };
        self.save(&record)?;
        let launched = self.launch(
            &mut record,
            &options,
            project_workspace.as_deref(),
            slot_command.as_deref(),
            relay_command.as_deref(),
            None,
        );
        if let Err(error) = launched {
            record.lifecycle = "launch_failed".to_owned();
            record.error = Some(if options.environment.is_empty() {
                error.to_string()
            } else {
                "launch failed with caller-supplied environment; details omitted from status"
                    .to_owned()
            });
            self.save(&record)?;
            return Err(fail(format!("launch of {agent_name:?} failed: {error}; record and any created tab retained at {}", directory.display())));
        }
        drop(identity_lock);
        // Keep this generation's lifecycle lock through startup delivery and returned status.
        // Sending by name after releasing it can redirect the old brief into a replacement.
        if let Some(brief) = options.brief {
            // A just-started harness may take longer than READBACK to print its first prompt;
            // it gets as long as it has to start working.
            let readback = Some(options.delivery.working_timeout);
            self.send_record(&record, &brief, options.delivery, None, readback)?;
        }
        self.status(agent_name)
    }

    /// Register an identity-checked live Herdr agent without owning its runtime.
    pub fn adopt(&self, agent_name: &str, options: AdoptOptions) -> Result<Value> {
        name(agent_name)?;
        if options.pane_id.is_empty() || options.pane_id.contains('\0') {
            return Err(fail("adopt needs a nonempty pane id without NUL"));
        }
        if options.expected_workspace.is_empty() || options.expected_workspace.contains('\0') {
            return Err(fail(
                "adopt needs a nonempty expected workspace label without NUL",
            ));
        }
        let project_workspace = self.project_workspace()?;
        if project_workspace
            .as_deref()
            .is_some_and(|workspace| workspace != options.expected_workspace)
        {
            return Err(fail(format!(
                "adopt workspace {:?} does not match project workspace {:?}",
                options.expected_workspace,
                project_workspace.as_deref().expect("checked above")
            )));
        }
        harness_arguments(&options.harness, None, None, &[])?;
        let cwd = fs::canonicalize(&options.cwd)
            .map_err(|_| fail(format!("cwd is not a directory: {}", options.cwd.display())))?;
        if !cwd.is_dir() {
            return Err(fail(format!("cwd is not a directory: {}", cwd.display())));
        }
        if options
            .session
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.contains('\0'))
        {
            return Err(fail(
                "native session id must be nonempty and contain no NUL",
            ));
        }
        let target = Target {
            pane_id: Some(options.pane_id.clone()),
            session_agent: options.session.as_ref().map(|_| options.harness.clone()),
            session_value: options.session.clone(),
            expected_agent: Some(options.harness.clone()),
            expected_workspace: Some(options.expected_workspace),
            expected_cwd: Some(cwd.clone()),
        };
        let _generation_lock = self.lock(agent_name)?;
        let directory = self.directory(agent_name)?;
        if fs::symlink_metadata(&directory).is_ok() {
            return Err(fail(format!(
                "agent {agent_name:?} already registered; stop it before reusing the name"
            )));
        }
        // Different names have independent lifecycle locks. Make adoption one
        // registry-wide identity transaction so simultaneous callers cannot
        // register two panes that report the same native session.
        let _identity_lock = self.identity_lock()?;
        self.refuse_pending_rename(&[agent_name])?;
        let (_target_lock, info) = agent::lock_resolved_target(self.client, &target)?;
        match (&info.session_agent, &info.session_value) {
            (None, None) => {}
            (Some(kind), Some(value))
                if !kind.is_empty()
                    && !value.is_empty()
                    && !kind.contains('\0')
                    && !value.contains('\0') =>
            {
                if kind != &options.harness {
                    return Err(fail(format!(
                        "refusing pane {}: native session agent is {kind:?}, expected {:?}",
                        options.pane_id, options.harness
                    )));
                }
            }
            (Some(_), Some(_)) => {
                return Err(fail(format!(
                    "refusing pane {}: native session identity is invalid",
                    options.pane_id
                )));
            }
            _ => {
                return Err(fail(format!(
                    "refusing pane {}: native session identity is incomplete",
                    options.pane_id
                )));
            }
        }
        let muse_identity = if options.harness == "muse" {
            if info
                .terminal_id
                .as_deref()
                .is_none_or(|value| !valid_metadata_text(value))
            {
                return Err(fail(
                    "adopting Muse requires an unchanged foreground harness and terminal identity",
                ));
            }
            let identity = self
                .client
                .harness_identity(&info.pane_id, &options.harness)?
                .ok_or_else(|| {
                    fail("adopting Muse requires a pinned sole foreground harness process")
                })?;
            if !identity.valid()
                || !self
                    .client
                    .verify_harness_identity(&info.pane_id, &identity)?
            {
                return Err(fail(
                    "adopting Muse requires an unchanged foreground harness and terminal identity",
                ));
            }
            Some(identity)
        } else {
            None
        };
        if info.session_value.is_some() {
            // A reported native session becomes the durable queue authority.
            // Prove now that it resolves uniquely back to this exact pane.
            agent::resolve_target(
                self.client,
                &Target {
                    pane_id: Some(info.pane_id.clone()),
                    session_agent: info.session_agent.clone(),
                    session_value: info.session_value.clone(),
                    expected_agent: Some(options.harness.clone()),
                    expected_workspace: None,
                    expected_cwd: Some(cwd.clone()),
                },
            )?;
        }
        let presentations: Vec<Pane> = self
            .client
            .panes()?
            .into_iter()
            .filter(|pane| pane.pane_id == info.pane_id)
            .collect();
        if presentations.len() != 1 {
            return Err(fail(format!(
                "refusing pane {}: expected one live presentation, found {}",
                options.pane_id,
                presentations.len()
            )));
        }
        let presentation = &presentations[0];
        if presentation.workspace_id != info.workspace_id {
            return Err(fail(format!(
                "refusing pane {}: presentation workspace identity changed",
                options.pane_id
            )));
        }
        let shell_identity = self.client.pane_shell_identity(&info.pane_id)?;
        if let Some(owner) = self.claim_owner(&info.pane_id, info.terminal_id.as_deref(), None)? {
            return Err(fail(format!(
                "pane {:?} is already registered as {owner:?}",
                options.pane_id
            )));
        }
        if let (Some(kind), Some(value)) = (&info.session_agent, &info.session_value) {
            if let Some(owner) = self.identity_owner(kind, value, None)? {
                return Err(fail(format!(
                    "native session is already registered as {:?}",
                    owner.name
                )));
            }
        }
        for entry in fs::read_dir(&self.registry).map_err(|error| fail(error.to_string()))? {
            let entry = entry.map_err(|error| fail(error.to_string()))?;
            let existing_name = entry.file_name().to_string_lossy().into_owned();
            if name(&existing_name).is_err() {
                continue;
            }
            let other = self.read_record(&existing_name)?;
            let same_session = info.session_value.is_some()
                && ((other.session_agent.as_deref().unwrap_or(&other.harness)
                    == info.session_agent.as_deref().unwrap_or("")
                    && (other.session_value == info.session_value
                        || other.goal_session_id == info.session_value
                        || other.resume == info.session_value))
                    || other.native_session.as_ref().is_some_and(|native| {
                        Some(&native.agent) == info.session_agent.as_ref()
                            && Some(&native.value) == info.session_value.as_ref()
                    }));
            let same_terminal = info.terminal_id.is_some() && other.terminal_id == info.terminal_id;
            if other.pane_id.as_ref() == Some(&info.pane_id) || same_session || same_terminal {
                return Err(fail(format!(
                    "pane {:?} is already registered as {:?}",
                    options.pane_id, other.name
                )));
            }
        }
        let confirmed = agent::resolve_target(
            self.client,
            &Target {
                pane_id: Some(info.pane_id.clone()),
                session_agent: info.session_agent.clone(),
                session_value: info.session_value.clone(),
                expected_agent: Some(options.harness.clone()),
                expected_workspace: None,
                expected_cwd: Some(cwd.clone()),
            },
        )?;
        if confirmed.workspace_id != info.workspace_id
            || confirmed.session_agent != info.session_agent
            || confirmed.session_value != info.session_value
        {
            return Err(fail(format!(
                "refusing pane {}: live identity changed before adoption",
                options.pane_id
            )));
        }
        let final_presentations: Vec<Pane> = self
            .client
            .panes()?
            .into_iter()
            .filter(|pane| pane.pane_id == confirmed.pane_id)
            .collect();
        if final_presentations.len() != 1
            || final_presentations[0].tab_id != presentation.tab_id
            || final_presentations[0].workspace_id != presentation.workspace_id
        {
            return Err(fail(format!(
                "refusing pane {}: live presentation changed before adoption",
                options.pane_id
            )));
        }
        if self.client.pane_shell_identity(&confirmed.pane_id)? != shell_identity {
            return Err(fail(format!(
                "refusing pane {}: pane shell process changed before adoption",
                options.pane_id
            )));
        }
        // Adoption is the operator's explicit assertion about this program, so its harness
        // process and terminal become the record's anchors.
        let harness_identity = if options.harness == "muse" {
            muse_identity
        } else {
            self.client
                .harness_identity(&confirmed.pane_id, &options.harness)?
        };
        if confirmed.terminal_id != info.terminal_id {
            return Err(fail(format!(
                "refusing pane {}: terminal changed before adoption",
                options.pane_id
            )));
        }
        if options.harness == "muse" {
            let identity = harness_identity.as_ref().ok_or_else(|| {
                fail("adopting Muse requires a pinned sole foreground harness process")
            })?;
            if !identity.valid()
                || confirmed
                    .terminal_id
                    .as_deref()
                    .is_none_or(|value| !valid_metadata_text(value))
                || !self
                    .client
                    .verify_harness_identity(&confirmed.pane_id, identity)?
            {
                return Err(fail(
                    "adopting Muse requires an unchanged foreground harness and terminal identity",
                ));
            }
        }
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&self.registry)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?;
        let mut record = AgentRecord {
            storage_directory: None,
            nested: None,
            adapter: "herdr-foreign".to_owned(),
            mode: interactive_mode(),
            backend: herdr_adapter(),
            paused: false,
            runtime_home: None,
            pane_reported_by_agentctl: false,
            custom_process_identity: None,
            foreign_shell_identity: Some(shell_identity.clone()),
            terminal_id: info.terminal_id,
            anchor_rule: harness_identity.as_ref().map(|_| ANCHOR_RULE),
            harness_identity,
            name_history: Vec::new(),
            former_names: Vec::new(),
            native_session: info
                .session_value
                .as_deref()
                .map(|value| NativeSession::new(&options.harness, value, "observed")),
            profile: None,
            slot: None,
            slot_project: None,
            slot_isolation: None,
            reasoning_effort: None,
            environment_names: Vec::new(),
            agentcloud: None,
            extra: BTreeMap::new(),
            name: agent_name.to_owned(),
            token: format!("{}-{}", now.as_nanos(), std::process::id()),
            harness: options.harness,
            cwd: cwd.display().to_string(),
            created_at: now.as_secs_f64(),
            schema: 1,
            lifecycle: "running".to_owned(),
            workspace_id: Some(info.workspace_id),
            tab_id: Some(presentation.tab_id.clone()),
            pane_id: Some(info.pane_id),
            session_agent: info.session_agent,
            session_value: info.session_value,
            model: None,
            resume: None,
            arguments: Vec::new(),
            startup_warning: None,
            effective_reasoning_effort: None,
            error: None,
            goal: None,
            goal_delivery: None,
            goal_session_id: None,
            goal_command: None,
            goal_messages: BTreeMap::new(),
            goal_message_id: None,
        };
        self.save(&record)?;
        let final_status = self.status_record(&record).and_then(|result| {
            if !result["probe_error"].is_null() {
                return Err(fail(format!(
                    "final live-identity verification failed: {}",
                    result["probe_error"]
                )));
            }
            let final_info = self.checked(&record)?;
            if final_info.session_agent != record.session_agent
                || final_info.session_value != record.session_value
            {
                return Err(fail("final live native-session identity changed"));
            }
            let live: Vec<Pane> = self
                .client
                .panes()?
                .into_iter()
                .filter(|pane| Some(&pane.pane_id) == record.pane_id.as_ref())
                .collect();
            if live.len() != 1
                || Some(&live[0].tab_id) != record.tab_id.as_ref()
                || Some(&live[0].workspace_id) != record.workspace_id.as_ref()
            {
                return Err(fail("final live-presentation verification failed"));
            }
            if self.client.pane_shell_identity(&confirmed.pane_id)? != shell_identity {
                return Err(fail("final pane shell process identity changed"));
            }
            Ok(result)
        });
        match final_status {
            Ok(result) => Ok(result),
            Err(error) => match self.archive_failed_adoption(&mut record, &error.to_string()) {
                Ok(destination) => Err(fail(format!(
                    "adoption failed final verification and was not registered; diagnostic record archived at {}: {error}",
                    destination.display()
                ))),
                Err(cleanup) => Err(fail(format!(
                    "adoption failed final verification ({error}); could not finish failed-record archival from {}: {cleanup}",
                    directory.display()
                ))),
            },
        }
    }

    fn archive_failed_adoption(&self, record: &mut AgentRecord, error: &str) -> Result<PathBuf> {
        let archive = self.registry.join("archive");
        agent::create_private_directory(&archive, "agent archive", false, false)?;
        let destination = archive.join(format!("{}-{}-adopt-failed", record.name, record.token));
        fs::rename(self.directory(&record.name)?, &destination)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&archive)?;
        agent::sync_directory(&self.registry)?;
        record.lifecycle = "adopt_failed".to_owned();
        record.error = Some(error.to_owned());
        agent::atomic_json(&destination.join("agent.json"), &json!(record))?;
        Ok(destination)
    }

    fn launch(
        &self,
        record: &mut AgentRecord,
        options: &StartOptions,
        project_workspace: Option<&str>,
        slot_command: Option<&str>,
        relay_command: Option<&str>,
        exclusion: Option<&revive::CensusExclusion>,
    ) -> Result<()> {
        self.create_presentation(record, options, project_workspace)?;
        let pane_id = record.pane_id.clone().expect("new tab has pane");
        if let Some(line) = slot_command {
            self.client
                .enter_slot_sandbox(&pane_id, line, options.startup_timeout)?;
        }
        if let Some(line) = relay_command {
            let harness = record.harness.clone();
            self.client.start_relay_agent(
                &harness,
                &pane_id,
                line,
                options.startup_timeout,
                &mut |identity| {
                    record.custom_process_identity = Some(identity);
                    self.save(record).map_err(|error| {
                        crate::error::AdapterError::unavailable(format!(
                            "cannot persist relay process identity: {error}"
                        ))
                    })
                },
            )?;
            record.pane_reported_by_agentctl = true;
            self.save(record)?;
        } else if record.adapter == "herdr-pane" {
            let agent_name = record.name.clone();
            let harness = record.harness.clone();
            let arguments = record.arguments.clone();
            self.client.start_pane_agent(
                &agent_name,
                &harness,
                &pane_id,
                &arguments,
                options.startup_timeout,
                &mut |identity| {
                    record.custom_process_identity = Some(identity);
                    self.save(record).map_err(|error| {
                        crate::error::AdapterError::unavailable(format!(
                            "cannot persist custom process identity: {error}"
                        ))
                    })
                },
            )?;
            record.pane_reported_by_agentctl = true;
            self.save(record)?;
            let screen = self.client.read(&pane_id, "visible", Some(200))?;
            (record.startup_warning, record.effective_reasoning_effort) =
                muse_startup_metadata(&screen);
        } else {
            self.client.start_agent(
                &record.name,
                &record.harness,
                &pane_id,
                &record.arguments,
                options.startup_timeout,
            )?;
        }
        let info = self.resolve_recovery_target(
            &WorkspaceClient {
                client: self.client,
                record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: None,
                check_prompt: true,
                expected_workspace: None,
                readback: None,
            },
            &self.target(record)?,
            exclusion,
            record.workspace_id.as_deref(),
        )?;
        record.capture_native_session(&info)?;
        let terminal_id = info.terminal_id;
        record.session_agent = info.session_agent;
        record.session_value = info.session_value;
        let owner = match (&record.session_agent, &record.session_value) {
            (Some(session_agent), Some(session_value)) => {
                self.identity_owner(session_agent, session_value, Some(&record.name))?
            }
            _ => None,
        };
        if let Some(owner) = owner {
            let pane = record.pane_id.as_deref().expect("launched pane");
            match self.client.close_pane(pane) {
                Ok(()) => {
                    record.session_agent = None;
                    record.session_value = None;
                    return Err(fail(format!(
                        "native session is already registered as {:?}; closed the conflicting new pane",
                        owner.name
                    )));
                }
                Err(error) => {
                    record.session_agent = None;
                    record.session_value = None;
                    return Err(fail(format!(
                        "native session is already registered as {:?}; could not close the conflicting new pane: {error}",
                        owner.name
                    )));
                }
            }
        }
        if record.session_value.is_some() {
            if let Err(error) =
                self.resolve_recovery_target(self.client, &self.target(record)?, exclusion, None)
            {
                record.session_agent = None;
                record.session_value = None;
                return Err(fail(format!(
                    "started native session is not globally unique; the failed owned pane remains available for stop: {error}"
                )));
            }
        }
        self.anchor_fresh(record, terminal_id)?;
        record.lifecycle = "running".to_owned();
        self.save(record)?;
        let final_info = match self
            .resolve_recovery_target(self.client, &self.target(record)?, exclusion, None)
            .and_then(|_| self.checked_with_recovery(record, exclusion))
        {
            Ok(info) => info,
            Err(error) => {
                record.session_agent = None;
                record.session_value = None;
                return Err(error);
            }
        };
        if final_info.session_agent != record.session_agent
            || final_info.session_value != record.session_value
        {
            record.session_agent = None;
            record.session_value = None;
            return Err(fail(
                "started agent native session changed during identity commit",
            ));
        }
        Ok(())
    }

    fn create_presentation(
        &self,
        record: &mut AgentRecord,
        options: &StartOptions,
        project_workspace: Option<&str>,
    ) -> Result<()> {
        self.create_presentation_in(
            record,
            options,
            project_workspace,
            project_workspace.unwrap_or("subagents"),
        )
    }

    /// Create the tab in the selected workspace, else in the one labelled `default_label`,
    /// creating that workspace when it does not exist.
    fn create_presentation_in(
        &self,
        record: &mut AgentRecord,
        options: &StartOptions,
        project_workspace: Option<&str>,
        default_label: &str,
    ) -> Result<()> {
        let _lock = agent::lock_target(
            &format!("managed-workspace:{default_label}"),
            "workspace allocation lock",
        )?;
        let selected = options
            .workspace_id
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if project_workspace.is_none() {
                    self.inherited_workspace
                        .clone()
                        .or_else(|| self.attached_self_workspace())
                } else {
                    None
                }
            });
        let selected = if let Some(workspace) = selected {
            let actual_label = self.client.workspace_label(&workspace)?;
            if project_workspace.is_some_and(|expected| actual_label != expected) {
                return Err(fail(format!(
                    "workspace {workspace:?} is labelled {actual_label:?}, but project configuration requires {default_label:?}"
                )));
            }
            Some(workspace)
        } else {
            self.client.workspace_id_for_label(default_label)?
        };
        match selected {
            Some(workspace) => {
                record.workspace_id = Some(workspace.clone());
                let (tab, pane) = self.client.create_tab_with_pane(
                    &workspace,
                    &record.name,
                    &record.cwd,
                    &options.environment,
                )?;
                record.tab_id = Some(tab);
                record.pane_id = Some(pane);
                self.save(record)?;
            }
            None => {
                let (workspace, tab, pane) = self.client.create_workspace(
                    default_label,
                    &record.cwd,
                    &options.environment,
                )?;
                record.workspace_id = Some(workspace);
                record.tab_id = Some(tab.clone());
                record.pane_id = Some(pane);
                self.save(record)?;
                self.client.rename_tab(&tab, &record.name)?;
            }
        }
        Ok(())
    }

    fn checked(&self, record: &AgentRecord) -> Result<AgentPaneInfo> {
        self.checked_with_recovery(record, None)
    }

    fn checked_with_recovery(
        &self,
        record: &AgentRecord,
        exclusion: Option<&revive::CensusExclusion>,
    ) -> Result<AgentPaneInfo> {
        self.resolve_recovery_target(
            &WorkspaceClient {
                client: self.client,
                record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: None,
                check_prompt: false,
                expected_workspace: None,
                readback: None,
            },
            &self.target(record)?,
            exclusion,
            record.workspace_id.as_deref(),
        )
    }

    fn checked_policy(&self, record: &AgentRecord, check_prompt: bool) -> Result<AgentPaneInfo> {
        agent::resolve_target(
            &WorkspaceClient {
                client: self.client,
                record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: None,
                check_prompt,
                expected_workspace: self.project_workspace()?,
                readback: None,
            },
            &self.target(record)?,
        )
    }

    /// The doctor finding and Herdr's refusal when Herdr positively reports the adopted
    /// record's workspace or pane as not found; `None` when it may exist.
    fn closed_foreign_target(&self, record: &AgentRecord) -> Option<(&'static str, String)> {
        if let Some(workspace) = record.workspace_id.as_deref() {
            if let Err(error) = self.client.tab_labels(workspace) {
                if let Some(missing) = missing_target(&error) {
                    return Some((missing, error.to_string()));
                }
            }
        }
        let error = self.client.pane_info(record.pane_id.as_deref()?).err()?;
        missing_target(&error).map(|missing| (missing, error.to_string()))
    }

    /// Archive an adopted record whose runtime Herdr reports closed; nothing is closed.
    /// There is no pane left to capture or revalidate, so the reason is kept beside the
    /// record as `stop.json`.
    fn archive_closed_foreign(
        &self,
        agent_name: &str,
        mut record: AgentRecord,
        missing: &str,
        detail: &str,
    ) -> Result<Value> {
        let (archive, destination) = self.archive_destination(&record)?;
        agent::atomic_json(
            &self.directory(agent_name)?.join("stop.json"),
            &json!({
                "source": missing, "detail": detail, "stopped_at": unix_seconds(),
                "pane_id": record.pane_id, "workspace_id": record.workspace_id,
            }),
        )?;
        record.lifecycle = "stopped".to_owned();
        self.save(&record)?;
        fs::rename(self.directory(agent_name)?, &destination)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&archive)?;
        agent::sync_directory(&self.registry)?;
        Ok(json!({
            "name": agent_name,
            "archive": destination,
            "pane_closed": false,
            "tab_closed": false,
            "source": missing,
        }))
    }

    fn inspect_foreign(
        &self,
        agent_name: &str,
        record: &AgentRecord,
    ) -> Result<(bool, AgentPaneInfo, Pane)> {
        self.inspect_foreign_with_stop_advice(agent_name, record, None)
    }

    fn inspect_foreign_with_stop_advice(
        &self,
        agent_name: &str,
        record: &AgentRecord,
        recovery: Option<&mut RecoveryAction>,
    ) -> Result<(bool, AgentPaneInfo, Pane)> {
        let pane_id = record
            .pane_id
            .as_deref()
            .ok_or_else(|| fail("adopted agent record has no pane identity"))?;
        let mut presentations: Vec<Pane> = self
            .client
            .panes()?
            .into_iter()
            .filter(|pane| pane.pane_id == pane_id)
            .collect();
        if presentations.len() != 1 {
            return Err(fail(format!(
                "refusing to unregister adopted agent {agent_name:?}: expected one recorded pane, found {}",
                presentations.len()
            )));
        }
        let presentation = presentations.pop().expect("one presentation was checked");
        if record.workspace_id.as_deref() != Some(presentation.workspace_id.as_str()) {
            return Err(fail(format!(
                "refusing to unregister adopted agent {agent_name:?}: recorded presentation workspace changed"
            )));
        }
        let info = self.client.pane_info(&presentation.pane_id)?;
        let cwd_matches = info.cwd == record.cwd
            || fs::canonicalize(&info.cwd)
                .ok()
                .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok());
        if info.pane_id != presentation.pane_id
            || Some(&info.workspace_id) != record.workspace_id.as_ref()
            || !cwd_matches
        {
            return Err(fail(format!(
                "refusing to unregister adopted agent {agent_name:?}: recorded pane, workspace, or cwd changed"
            )));
        }
        let Some(shell_identity) = record.foreign_shell_identity.as_ref() else {
            if let Some(recovery) = recovery {
                if matches!(recovery, RecoveryAction::Doctor) {
                    if let Some(action) = self.legacy_stop_recovery(record, &info, &presentation)? {
                        *recovery = action;
                    }
                }
            }
            return Err(fail(format!(
                "refusing to unregister adopted agent {agent_name:?}: legacy record has no identity-bound pane shell"
            )));
        };
        if self.client.pane_shell_identity(&info.pane_id)? != *shell_identity {
            return Err(fail(format!(
                "refusing to unregister adopted agent {agent_name:?}: recorded pane shell generation changed"
            )));
        }
        if info.agent.is_none() {
            // A live agent is pinned by its harness and, when one was
            // reported, native session. A returned shell has no such process
            // identity, so its recorded tab remains part of the fallback
            // proof. This still lets an operator unregister a fully
            // revalidated live agent after moving its pane between tabs in the
            // same workspace.
            if record.tab_id.as_deref() != Some(presentation.tab_id.as_str()) {
                return Err(fail(format!(
                    "refusing to unregister adopted agent {agent_name:?}: recorded tab changed while the agent was absent"
                )));
            }
            if info.session_agent.is_some() || info.session_value.is_some() {
                return Err(fail(format!(
                    "refusing pane {}: absent agent has native session identity",
                    info.pane_id
                )));
            }
            if !self
                .client
                .pane_is_same_idle_shell(&info.pane_id, shell_identity)?
            {
                return Err(fail(format!(
                    "refusing pane {}: absent agent is not at the recorded identity-bound idle shell process group",
                    info.pane_id
                )));
            }
            return Ok((false, info, presentation));
        }
        Ok((true, self.checked(record)?, presentation))
    }

    fn legacy_stop_recovery(
        &self,
        record: &AgentRecord,
        info: &AgentPaneInfo,
        presentation: &Pane,
    ) -> Result<Option<RecoveryAction>> {
        if record.adapter != "herdr-foreign"
            || record.lifecycle != "running"
            || record.mode != "interactive"
            || record.backend != "herdr"
            || info.agent.is_some()
            || info.session_agent.is_some()
            || info.session_value.is_some()
            || record.tab_id.as_ref() != Some(&presentation.tab_id)
        {
            return Ok(None);
        }
        let pinned = self.pinned_agent_directory(&record.name)?;
        let snapshot = self.managed_record_snapshot(&pinned, &record.token)?;
        if &snapshot.record != record {
            return Err(fail(format!(
                "agent {:?} record changed before stop recovery advice",
                record.name
            )));
        }
        let document: Value = serde_json::from_slice(&snapshot.content)
            .map_err(|_| fail("invalid legacy agent record before stop recovery advice"))?;
        if document.get("foreign_shell_identity").is_some() {
            return Ok(None);
        }
        if self
            .dead_pane_proof(record, "prepare legacy stop recovery advice")
            .is_err()
        {
            return Ok(None);
        }
        if self.record_bytes(&pinned)? != snapshot.content {
            return Err(fail(format!(
                "agent {:?} record changed during stop recovery advice",
                record.name
            )));
        }
        Ok(Some(RecoveryAction::Stop {
            name: record.name.clone(),
            token: record.token.clone(),
            record_sha256: Some(format!("{:x}", Sha256::digest(&snapshot.content))),
            skip_cloud_halt: false,
        }))
    }

    /// Report live state or an explicit probe error without reaping durable records.
    pub fn status(&self, agent_name: &str) -> Result<Value> {
        self.status_record(&self.load(agent_name)?)
    }

    fn status_record_listed(
        &self,
        record: &AgentRecord,
        fleet: &mut cloud::FleetCache,
    ) -> Result<Value> {
        if record.is_cloud() {
            self.cloud_status_record(record, fleet)
        } else {
            self.status_record(record)
        }
    }

    /// Resolve and verify the exact live pane owned by one registered agent.
    pub fn pane_info(&self, agent_name: &str) -> Result<AgentPaneInfo> {
        self.checked(&self.load(agent_name)?)
    }

    /// Move one owned interactive agent into the workspace required by the
    /// project configuration, preserving its process and native session.
    ///
    /// Herdr changes public pane IDs during cross-workspace moves. If Herdr
    /// completed the move but the registry write was interrupted, repeating
    /// this operation recovers through the globally unique managed agent name
    /// and commits the already-moved identity.
    pub fn move_to_project_workspace(&self, agent_name: &str) -> Result<Value> {
        let _generation_lock = self.lock(agent_name)?;
        let mut record = self.load(agent_name)?;
        record.supported()?;
        if record.adapter != "herdr" || record.lifecycle != "running" {
            return Err(fail(
                "move supports only a running agentctl-owned native Herdr agent",
            ));
        }
        let project_workspace = self.project_workspace()?;
        let (destination, expected_workspace) = if let Some(expected) = project_workspace {
            let destination = self
                .client
                .workspace_id_for_label(&expected)?
                .ok_or_else(|| {
                    fail(format!(
                        "configured project workspace {expected:?} does not exist"
                    ))
                })?;
            if self.client.workspace_label(&destination)? != expected {
                return Err(fail(
                    "configured project workspace changed during resolution",
                ));
            }
            (destination, expected)
        } else {
            let destination = self.pending_move_destination(&record)?.ok_or_else(|| {
                fail(
                    "move requires a workspace field in .agentctl/profiles.json or a durable pending move",
                )
            })?;
            let expected = self.client.workspace_label(&destination)?;
            (destination, expected)
        };

        let recorded_pane = record
            .pane_id
            .clone()
            .ok_or_else(|| fail(format!("agent {agent_name:?} has no confirmed pane")))?;
        let previous_target = record.target()?;
        let named_pane = self.client.agent_pane(agent_name)?;
        if named_pane != recorded_pane {
            self.require_move_intent(&record, &destination)?;
            if self
                .client
                .panes()?
                .iter()
                .any(|pane| pane.pane_id == recorded_pane)
            {
                return Err(fail(format!(
                    "refusing to recover move of {agent_name:?}: both recorded and named panes are live"
                )));
            }
            let target = Target {
                pane_id: Some(named_pane.clone()),
                session_agent: record.session_agent.clone(),
                session_value: record.session_value.clone(),
                expected_agent: Some(record.harness.clone()),
                expected_workspace: Some(expected_workspace.clone()),
                expected_cwd: Some(PathBuf::from(&record.cwd)),
            };
            let mut binding_target = record.target()?;
            binding_target.pane_id = Some(named_pane.clone());
            let (info, presentation) = agent::rebind_queue_after(
                &self.queue(agent_name)?,
                &previous_target,
                Some(&binding_target),
                true,
                || {
                    let (_target_lock, info) = agent::lock_resolved_target(self.client, &target)?;
                    if self.client.agent_pane(agent_name)? != named_pane {
                        return Err(fail(format!(
                            "refusing to recover move of {agent_name:?}: managed name changed panes"
                        )));
                    }
                    if self
                        .client
                        .panes()?
                        .iter()
                        .any(|pane| pane.pane_id == recorded_pane)
                    {
                        return Err(fail(format!(
                            "refusing to recover move of {agent_name:?}: both recorded and named panes are live"
                        )));
                    }
                    let mut presentations: Vec<Pane> = self
                        .client
                        .panes()?
                        .into_iter()
                        .filter(|pane| {
                            pane.workspace_id == destination && pane.pane_id == info.pane_id
                        })
                        .collect();
                    if presentations.len() != 1 {
                        return Err(fail(format!(
                            "refusing to recover move of {agent_name:?}: expected one destination presentation, found {}",
                            presentations.len()
                        )));
                    }
                    let presentation = presentations.pop().expect("one presentation");
                    if self
                        .client
                        .panes()?
                        .iter()
                        .filter(|pane| {
                            pane.workspace_id == destination && pane.tab_id == presentation.tab_id
                        })
                        .count()
                        != 1
                    {
                        return Err(fail(format!(
                            "refusing to recover move of {agent_name:?}: its tab contains another pane"
                        )));
                    }
                    Ok((binding_target.clone(), (info, presentation)))
                },
            )?;
            record.workspace_id = Some(destination.clone());
            record.tab_id = Some(presentation.tab_id);
            record.pane_id = Some(info.pane_id);
            self.repin_moved_terminal(&mut record)?;
            self.save(&record)?;
            self.clear_move_intent(agent_name)?;
            let mut result = self.status_record(&record)?;
            result["moved"] = json!(true);
            result["recovered"] = json!(true);
            result["previous_pane_id"] = json!(recorded_pane);
            return Ok(result);
        }

        let moved = agent::rebind_queue_after(
            &self.queue(agent_name)?,
            &previous_target,
            None,
            true,
            || {
                let source_client = WorkspaceClient {
                    client: self.client,
                    record: &record,
                    goal_objective: Mutex::new(None),
                    custom_submission: Mutex::new(None),
                    queue: None,
                    check_prompt: true,
                    expected_workspace: None,
                    readback: None,
                };
                let (_target_lock, source_info) =
                    agent::lock_resolved_target(&source_client, &previous_target)?;
                if self.client.agent_pane(agent_name)? != source_info.pane_id {
                    return Err(fail(format!(
                        "refusing to move {agent_name:?}: managed agent name changed panes"
                    )));
                }
                let mut presentations: Vec<Pane> = self
                    .client
                    .panes()?
                    .into_iter()
                    .filter(|pane| {
                        pane.workspace_id == source_info.workspace_id
                            && pane.pane_id == source_info.pane_id
                    })
                    .collect();
                if presentations.len() != 1 {
                    return Err(fail(format!(
                        "refusing to move {agent_name:?}: expected one source presentation, found {}",
                        presentations.len()
                    )));
                }
                let source = presentations.pop().expect("one source presentation");
                if Some(&source.tab_id) != record.tab_id.as_ref()
                    || Some(&source.workspace_id) != record.workspace_id.as_ref()
                {
                    return Err(fail(format!(
                        "refusing to move {agent_name:?}: recorded tab or workspace identity changed"
                    )));
                }
                if self
                    .client
                    .panes()?
                    .iter()
                    .filter(|pane| {
                        pane.workspace_id == source.workspace_id && pane.tab_id == source.tab_id
                    })
                    .count()
                    != 1
                {
                    return Err(fail(format!(
                        "refusing to move {agent_name:?}: its tab contains another pane"
                    )));
                }
                if source.workspace_id == destination {
                    let target = Target {
                        pane_id: Some(source.pane_id),
                        session_agent: record.session_agent.clone(),
                        session_value: record.session_value.clone(),
                        expected_agent: Some(record.harness.clone()),
                        expected_workspace: None,
                        expected_cwd: Some(PathBuf::from(&record.cwd)),
                    };
                    return Ok((target, None));
                }
                self.write_move_intent(&record, &destination)?;
                let moved = match self.client.move_pane_to_new_tab(
                    &source.pane_id,
                    &destination,
                    agent_name,
                ) {
                    Ok(moved) => moved,
                    Err(error) => {
                        if self
                            .source_unchanged_after_failed_move(&record)
                            .unwrap_or(false)
                        {
                            self.clear_move_intent(agent_name)?;
                        }
                        return Err(agent::AgentError::Client(error));
                    }
                };
                if self.client.agent_pane(agent_name)? != moved.pane_id {
                    return Err(fail(format!(
                        "move of {agent_name:?} completed but the managed name did not follow it; rerun move only after inspecting Herdr"
                    )));
                }
                let target = Target {
                    pane_id: Some(moved.pane_id.clone()),
                    session_agent: record.session_agent.clone(),
                    session_value: record.session_value.clone(),
                    expected_agent: Some(record.harness.clone()),
                    expected_workspace: Some(expected_workspace.clone()),
                    expected_cwd: Some(PathBuf::from(&record.cwd)),
                };
                let info = agent::resolve_target(self.client, &target)?;
                let final_presentations: Vec<Pane> = self
                    .client
                    .panes()?
                    .into_iter()
                    .filter(|pane| pane.workspace_id == destination && pane.pane_id == info.pane_id)
                    .collect();
                if final_presentations.len() != 1
                    || final_presentations[0].tab_id != moved.tab_id
                    || final_presentations[0].workspace_id != moved.workspace_id
                {
                    return Err(fail(format!(
                        "move of {agent_name:?} completed but final presentation verification failed; rerun move to recover"
                    )));
                }
                let mut binding_target = target;
                binding_target.expected_workspace = None;
                Ok((binding_target, Some(moved)))
            },
        )?;
        let Some(moved) = moved else {
            self.clear_move_intent(agent_name)?;
            let mut result = self.status_record(&record)?;
            result["moved"] = json!(false);
            result["recovered"] = json!(false);
            return Ok(result);
        };
        record.workspace_id = Some(moved.workspace_id);
        record.tab_id = Some(moved.tab_id);
        record.pane_id = Some(moved.pane_id);
        self.repin_moved_terminal(&mut record)?;
        self.save(&record)?;
        self.clear_move_intent(agent_name)?;
        let mut result = self.status_record(&record)?;
        result["moved"] = json!(true);
        result["recovered"] = json!(false);
        result["previous_pane_id"] = json!(recorded_pane);
        Ok(result)
    }

    /// Resolve the exact live pane while allowing a service owner to cancel control waits.
    pub(crate) fn pane_info_with_runtime(
        &self,
        agent_name: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<AgentPaneInfo> {
        let _lock = self.lock_with_runtime(agent_name, runtime)?;
        let record = self.load(agent_name)?;
        agent::resolve_target_with_runtime(
            &WorkspaceClient {
                client: self.client,
                record: &record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: None,
                check_prompt: false,
                expected_workspace: None,
                readback: None,
            },
            &self.target(&record)?,
            runtime,
        )
    }

    fn status_record(&self, record: &AgentRecord) -> Result<Value> {
        if record.is_cloud() {
            return self.cloud_status_record(record, &mut cloud::FleetCache::new());
        }
        let agent_name = &record.name;
        let mut result = record.document()?;
        result["queue"] = json!(self.queue(agent_name)?);
        result["output"] = json!(self.directory(agent_name)?.join("output.json"));
        result["goal_source"] = if record.goal.is_some() {
            json!("requested")
        } else {
            Value::Null
        };
        result["goal_delivery"] = json!(self.goal_delivery(record));
        match self.pending_move_destination(record) {
            Ok(Some(destination)) => {
                result["agent_status"] = json!("unknown");
                result["move_pending"] = json!(true);
                result["move_destination_workspace_id"] = json!(destination);
                result["probe_error"] = json!(format!(
                    "move of {:?} is incomplete; rerun `agentctl move {}`",
                    record.name, record.name
                ));
                return Ok(result);
            }
            Ok(None) => {}
            Err(error) => {
                result["agent_status"] = json!("unknown");
                result["move_pending"] = json!(true);
                result["probe_error"] = json!(error.to_string());
                return Ok(result);
            }
        }
        let client = WorkspaceClient {
            client: self.client,
            record,
            goal_objective: Mutex::new(None),
            custom_submission: Mutex::new(None),
            queue: None,
            check_prompt: false,
            expected_workspace: None,
            readback: None,
        };
        let probe = self.target(record).and_then(|target| {
            agent::resolve_target(&client, &target)
                .and_then(|_| agent::status_managed(&client, &target, &self.queue(agent_name)?))
        });
        match probe {
            Ok(status) => {
                result
                    .as_object_mut()
                    .expect("record object")
                    .extend(json!(status).as_object().expect("status object").clone());
                result["probe_error"] = Value::Null;
            }
            Err(error) => {
                result["agent_status"] = json!("unknown");
                result["probe_error"] = json!(error.to_string());
            }
        }
        Ok(result)
    }

    /// List every registered agent, preserving records when Herdr is unavailable.
    pub fn list(&self) -> Result<Vec<Value>> {
        if !self.registry.exists() {
            return Ok(Vec::new());
        }
        agent::validate_private_directory(&self.registry, "agent registry", false)?;
        let mut names = fs::read_dir(&self.registry)
            .map_err(|error| fail(error.to_string()))?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| fail(error.to_string()))?;
        names.retain(|value| name(value).is_ok());
        names.sort();
        // One non-attaching agentcloud listing serves every cloud agent in this pass.
        let mut fleet = cloud::FleetCache::new();
        // Each row stands alone: a record this build cannot read is reported as its own error row
        // instead of hiding every other agent in the registry.
        Ok(names
            .iter()
            .map(|name| {
                self.load(name)
                    .and_then(|record| self.status_record_listed(&record, &mut fleet))
                    .unwrap_or_else(|error| {
                        json!({
                            "name": name,
                            "agent_status": "unknown",
                            "probe_error": error.to_string(),
                            "record_error": true,
                        })
                    })
            })
            .collect())
    }

    /// Serialize against stop and durably deliver one prompt.
    pub fn send(&self, agent_name: &str, text: &str, options: DrainOptions) -> Result<QueueResult> {
        self.send_identified(agent_name, text, options, None)
    }

    /// Submit a prompt with an optional caller-supplied ID; existing IDs are refused.
    pub fn send_identified(
        &self,
        agent_name: &str,
        text: &str,
        options: DrainOptions,
        message_id: Option<&str>,
    ) -> Result<QueueResult> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        self.send_record(&record, text, options, message_id, None)
    }

    /// Submit through an injected runtime that can interrupt bounded readiness waits.
    pub(crate) fn send_identified_with_runtime(
        &self,
        agent_name: &str,
        text: &str,
        options: DrainOptions,
        message_id: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<QueueResult> {
        let _lock = self.lock_with_runtime(agent_name, runtime)?;
        let record = self.load(agent_name)?;
        record.input_allowed()?;
        let queue = self.queue(&record.name)?;
        let client = WorkspaceClient {
            client: self.client,
            record: &record,
            goal_objective: Mutex::new(None),
            custom_submission: Mutex::new(None),
            queue: Some(&queue),
            check_prompt: true,
            expected_workspace: self.project_workspace()?,
            readback: None,
        };
        agent::send_identified_managed_with_runtime(
            &client,
            &self.target(&record)?,
            &queue,
            text,
            options,
            runtime,
            Some(message_id),
        )
    }

    /// Reconcile one caller-selected durable delivery identifier by exact path lookup.
    pub fn message_state(
        &self,
        agent_name: &str,
        message_id: &str,
    ) -> Result<Option<agent::QueueMessageState>> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        record.supported()?;
        agent::message_state(&self.queue(agent_name)?, message_id)
    }

    pub(crate) fn message_state_with_runtime(
        &self,
        agent_name: &str,
        message_id: &str,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<Option<agent::QueueMessageState>> {
        let _lock = self.lock_with_runtime(agent_name, runtime)?;
        let record = self.load(agent_name)?;
        record.supported()?;
        agent::message_state(&self.queue(agent_name)?, message_id)
    }

    fn send_record(
        &self,
        record: &AgentRecord,
        text: &str,
        options: DrainOptions,
        message_id: Option<&str>,
        readback: Option<Duration>,
    ) -> Result<QueueResult> {
        record.input_allowed()?;
        let queue = self.queue(&record.name)?;
        let client = WorkspaceClient {
            client: self.client,
            record,
            goal_objective: Mutex::new(None),
            custom_submission: Mutex::new(None),
            queue: Some(&queue),
            check_prompt: true,
            expected_workspace: self.project_workspace()?,
            readback,
        };
        match message_id {
            Some(identifier) => agent::send_identified_managed(
                &client,
                &self.target(record)?,
                &queue,
                text,
                options,
                identifier,
            ),
            None => agent::send_managed(&client, &self.target(record)?, &queue, text, options),
        }
    }

    /// Hand input ownership to a human, without signaling or suspending the harness.
    pub fn pause(&self, agent_name: &str, paused: bool) -> Result<Value> {
        let _lock = self.lock(agent_name)?;
        let mut record = self.load(agent_name)?;
        if !record.is_cloud() {
            record.supported()?;
        }
        record.paused = paused;
        self.save(&record)?;
        Ok(json!({"name": agent_name, "token": record.token, "paused": paused}))
    }

    /// Verify the live agent identity, then focus its pane without changing input ownership.
    pub fn attach(&self, agent_name: &str) -> Result<Value> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        if record.is_cloud() {
            return self.cloud_attach(&record);
        }
        let info = self.checked(&record)?;
        self.client.focus_pane(&info.pane_id)?;
        Ok(json!({"name": agent_name, "pane_id": info.pane_id, "paused": record.paused}))
    }

    /// Drain only prompts known not to have been injected.
    pub fn drain(&self, agent_name: &str, options: DrainOptions) -> Result<QueueResult> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        record.input_allowed()?;
        agent::drain_managed(
            &WorkspaceClient {
                client: self.client,
                record: &record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: Some(&self.queue(agent_name)?),
                check_prompt: true,
                expected_workspace: self.project_workspace()?,
                readback: None,
            },
            &self.target(&record)?,
            &self.queue(agent_name)?,
            options,
        )
    }

    /// Drain through an injected runtime that can interrupt bounded readiness waits.
    pub(crate) fn drain_with_runtime(
        &self,
        agent_name: &str,
        options: DrainOptions,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<QueueResult> {
        let _lock = self.lock_with_runtime(agent_name, runtime)?;
        let record = self.load(agent_name)?;
        record.input_allowed()?;
        let queue = self.queue(&record.name)?;
        agent::drain_managed_with_runtime(
            &WorkspaceClient {
                client: self.client,
                record: &record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: Some(&queue),
                check_prompt: true,
                expected_workspace: self.project_workspace()?,
                readback: None,
            },
            &self.target(&record)?,
            &queue,
            options,
            runtime,
        )
    }

    fn snapshot(&self, record: &AgentRecord, text: &str) -> Result<()> {
        let captured = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?
            .as_secs_f64();
        agent::atomic_json(
            &self.directory(&record.name)?.join("output.json"),
            &json!({"text":text,"captured_at":captured,"pane_id":record.pane_id}),
        )
    }

    /// Read the shared terminal and persist the latest bounded snapshot.
    pub fn read(&self, agent_name: &str, lines: usize) -> Result<String> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        self.checked_policy(&record, false)?;
        let text = agent::read(self.client, &self.target(&record)?, lines)?;
        self.snapshot(&record, &text)?;
        Ok(text)
    }

    /// Read what a chat bridge scans for replies and persist it as the latest bounded snapshot:
    /// see [`agent::read_capture`].
    pub(crate) fn read_capture(&self, agent_name: &str, lines: usize) -> Result<String> {
        let _lock = self.lock(agent_name)?;
        let record = self.load(agent_name)?;
        self.checked_policy(&record, false)?;
        let text = agent::read_capture(self.client, &self.target(&record)?, lines)?;
        self.snapshot(&record, &text)?;
        Ok(text)
    }

    /// Read what a chat bridge scans for replies, from the source [`chat_capture_source`] picks,
    /// and persist it as the latest bounded snapshot, while allowing service cancellation during
    /// locks and control.
    pub(crate) fn read_capture_with_runtime(
        &self,
        agent_name: &str,
        lines: usize,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<String> {
        self.capture_with_runtime(agent_name, lines, runtime, true)
    }

    /// Read as [`Self::read_capture_with_runtime`] does, without persisting a snapshot: for a read
    /// the service repeats every few seconds, where each snapshot would cost a file write and two
    /// fsyncs.
    pub(crate) fn peek_capture_with_runtime(
        &self,
        agent_name: &str,
        lines: usize,
        runtime: &dyn agent::AgentRuntime,
    ) -> Result<String> {
        self.capture_with_runtime(agent_name, lines, runtime, false)
    }

    fn capture_with_runtime(
        &self,
        agent_name: &str,
        lines: usize,
        runtime: &dyn agent::AgentRuntime,
        persist: bool,
    ) -> Result<String> {
        let _lock = self.lock_with_runtime(agent_name, runtime)?;
        let record = self.load(agent_name)?;
        let target = self.policy_target(&record)?;
        let info = agent::resolve_target_with_runtime(
            &WorkspaceClient {
                client: self.client,
                record: &record,
                goal_objective: Mutex::new(None),
                custom_submission: Mutex::new(None),
                queue: None,
                check_prompt: false,
                expected_workspace: None,
                readback: None,
            },
            &target,
            runtime,
        )?;
        let source = chat_capture_source(&info);
        let mut text =
            self.client
                .read_with_runtime(&info.pane_id, source, Some(lines), runtime)?;
        if text.is_empty() && source == "recent-unwrapped" {
            text = self
                .client
                .read_with_runtime(&info.pane_id, "recent", Some(lines), runtime)?;
        }
        if persist {
            self.snapshot(&record, &text)?;
        }
        Ok(text)
    }

    /// Wait for readiness, which is separate from completion of the agent's goal.
    pub fn wait(&self, agent_name: &str, timeout: Duration) -> Result<Value> {
        self.wait_at(agent_name, timeout, &unix_seconds)
    }

    /// Wait for readiness, reading wall-clock seconds from `wall`.
    ///
    /// A screen-verified delivery proves the prompt left the composer, but the terminal server can
    /// report the agent idle until the harness visibly starts the turn. So an idle state within
    /// `DELIVERY_SETTLE_SECONDS` of the newest confirmed delivery counts only after this wait has
    /// seen the agent busy.
    fn wait_at(
        &self,
        agent_name: &str,
        timeout: Duration,
        wall: &dyn Fn() -> f64,
    ) -> Result<Value> {
        if timeout > Duration::from_secs(31_536_000) {
            return Err(fail(
                "wait timeout must be finite and between 0 and 31536000 seconds",
            ));
        }
        let initial = self.load(agent_name)?;
        if initial.is_cloud() {
            return self.cloud_wait(agent_name, timeout);
        }
        let token = initial.token;
        let delivered_at = latest_confirmed_delivery(&self.queue(agent_name)?);
        let mut seen_busy = false;
        let start = Instant::now();
        loop {
            let lock = self.lock(agent_name)?;
            let record = self.load(agent_name)?;
            if record.token != token {
                return Err(fail(format!(
                    "agent {agent_name:?} was replaced while waiting"
                )));
            }
            let info = self.checked(&record)?;
            let idle = matches!(info.status.as_str(), "idle" | "done");
            if matches!(info.status.as_str(), "working" | "starting") {
                seen_busy = true;
            }
            if idle {
                let settled = delivered_at
                    .is_none_or(|delivered| wall() - delivered >= DELIVERY_SETTLE_SECONDS);
                if seen_busy || settled {
                    return self.status_record(&record);
                }
            } else if !matches!(info.status.as_str(), "working" | "starting" | "unknown") {
                return Err(fail(format!(
                    "agent {agent_name:?} requires attention (state {:?}); read its pane",
                    info.status
                )));
            }
            if start.elapsed() >= timeout {
                if idle {
                    let ago = (wall() - delivered_at.unwrap_or_default()).max(0.0);
                    return Err(fail(format!(
                        "timed out waiting for agent {agent_name:?}: it reads {:?}, but a prompt was \
                         delivered {ago:.1}s ago and the agent has not been seen working since",
                        info.status
                    )));
                }
                return Err(fail(format!(
                    "timed out waiting for agent {agent_name:?} (state {:?})",
                    info.status
                )));
            }
            drop(lock);
            std::thread::sleep(
                Duration::from_millis(250).min(timeout.saturating_sub(start.elapsed())),
            );
        }
    }

    /// Bind an explicitly known native session without changing the existing queue identity.
    pub fn bind_session(
        &self,
        agent_name: &str,
        session_id: &str,
        goal_command: Option<&[String]>,
    ) -> Result<Value> {
        if session_id.is_empty() || session_id.contains('\0') {
            return Err(fail(
                "native session id must be nonempty and contain no NUL",
            ));
        }
        if goal_command.is_some_and(|command| {
            command.is_empty()
                || command
                    .iter()
                    .any(|item| item.is_empty() || item.contains('\0'))
        }) {
            return Err(fail("goal command must be a nonempty argument vector"));
        }
        let _lock = self.lock(agent_name)?;
        let _identity_lock = self.identity_lock()?;
        let mut record = self.load(agent_name)?;
        let info = self.checked(&record)?;
        let recovery_session = record
            .native_session
            .as_ref()
            .map(|native| native.value.clone());
        for existing in [
            &record.goal_session_id,
            &record.session_value,
            &info.session_value,
            &record.resume,
            &recovery_session,
        ]
        .into_iter()
        .flatten()
        {
            if existing != session_id {
                return Err(fail("refusing to replace an already bound native session"));
            }
        }
        if let Some(owner) = self.identity_owner(&record.harness, session_id, Some(agent_name))? {
            return Err(fail(format!(
                "native session is already registered as {:?}",
                owner.name
            )));
        }
        let mut reported = Vec::new();
        for pane in self.client.panes()? {
            let live = self.client.pane_info(&pane.pane_id)?;
            if live.session_agent.as_deref() == Some(&record.harness)
                && live.session_value.as_deref() == Some(session_id)
            {
                reported.push(pane.pane_id);
            }
        }
        if !reported.is_empty()
            && (reported.len() != 1 || Some(&reported[0]) != record.pane_id.as_ref())
        {
            return Err(fail(
                "native session is reported by another or ambiguous live pane",
            ));
        }
        // Session metadata is optional in Herdr. This is an explicit caller
        // assertion anchored to the independently verified live name and pane.
        record.bind_asserted_session(session_id);
        if record.native_session.is_none() {
            record.native_session =
                Some(NativeSession::new(&record.harness, session_id, "asserted"));
        }
        if let Some(command) = goal_command {
            record.goal_command = Some(command.to_vec());
        }
        self.save(&record)?;
        Ok(json!({"name":agent_name,"session_id":session_id,"source":"explicit"}))
    }

    fn goal_delivery(&self, record: &AgentRecord) -> Option<String> {
        if let (Some(identifier), Ok(queue)) = (&record.goal_message_id, self.queue(&record.name)) {
            for (folder, outcome) in [
                ("processed", "delivered"),
                ("failed", "possibly_submitted"),
                ("inflight", "possibly_submitted"),
                ("inbox", "pending"),
            ] {
                if fs::symlink_metadata(queue.join(folder).join(format!("{identifier}.json")))
                    .is_ok()
                {
                    return Some(outcome.to_owned());
                }
            }
        }
        record.goal_delivery.clone()
    }

    fn goal_result(&self, record: &AgentRecord, command: Option<&[String]>) -> Value {
        let mut result = json!({"name":record.name,"goal":record.goal,"delivery":self.goal_delivery(record),"source":"requested","native_status":"unverified"});
        if record.harness != "codex" {
            return result;
        }
        let Some(session) = record
            .goal_session_id
            .as_deref()
            .or(record.session_value.as_deref())
            .filter(|s| !s.is_empty())
        else {
            result["native_error"] = json!(
                "native session unknown; bind-session with the session id reported by this agent"
            );
            return result;
        };
        let default = [
            "codex".to_owned(),
            "app-server".to_owned(),
            "proxy".to_owned(),
        ];
        let command = command
            .or(record.goal_command.as_deref())
            .unwrap_or(&default);
        match crate::codex_goal::get_goal(session, command, Duration::from_secs(30)) {
            Ok(native) => {
                result["source"] = json!("native");
                result["native_status"] = native
                    .as_ref()
                    .map_or(json!("absent"), |goal| goal["status"].clone());
                result["goal"] = native
                    .as_ref()
                    .map_or(Value::Null, |goal| goal["objective"].clone());
                result["native"] = json!(native);
            }
            Err(error) => result["native_error"] = json!(error),
        }
        result
    }

    /// Inspect a native Codex goal when bound, or submit a visible goal and wake its harness.
    pub fn goal(
        &self,
        agent_name: &str,
        text: Option<&str>,
        options: DrainOptions,
        goal_command: Option<&[String]>,
    ) -> Result<Value> {
        let Some(text) = text else {
            let record = self.load(agent_name)?;
            self.checked(&record)?;
            return Ok(self.goal_result(&record, goal_command));
        };
        if text.trim().is_empty() || text.contains(['\n', '\r']) {
            return Err(fail("goal must be a nonempty single line"));
        }
        let _lock = self.lock(agent_name)?;
        let mut record = self.load(agent_name)?;
        record.input_allowed()?;
        record.goal = Some(text.to_owned());
        record.goal_delivery = Some("pending".to_owned());
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?
            .as_nanos();
        let identifier = format!("{timestamp:020}-{}", std::process::id());
        record.goal_message_id = Some(identifier.clone());
        if record.harness == "codex" {
            record
                .goal_messages
                .insert(identifier.clone(), text.to_owned());
        }
        self.save(&record)?;
        let prompt = if record.harness == "codex" {
            format!("/goal {text}")
        } else {
            format!("Your ongoing goal: {text}\nWork toward this goal and report completion or blockers.")
        };
        let queue = self.queue(agent_name)?;
        let client = WorkspaceClient {
            client: self.client,
            record: &record,
            goal_objective: Mutex::new(None),
            custom_submission: Mutex::new(None),
            queue: Some(&queue),
            check_prompt: true,
            expected_workspace: self.project_workspace()?,
            readback: None,
        };
        let outcome = agent::send_identified_managed(
            &client,
            &self.target(&record)?,
            &queue,
            &prompt,
            options,
            &identifier,
        );
        match outcome {
            Ok(delivered) => {
                record.goal_delivery = Some(delivered.outcome.as_str().to_owned());
                self.save(&record)?;
                Ok(self.goal_result(&record, goal_command))
            }
            Err(error) => {
                record.goal_delivery = Some(
                    error
                        .outcome()
                        .map_or("failed", |outcome| outcome.as_str())
                        .to_owned(),
                );
                self.save(&record)?;
                Err(error)
            }
        }
    }

    fn checked_or_launch_failed(&self, record: &AgentRecord, pane: &str) -> Result<()> {
        if record.adapter == "herdr-relay"
            && matches!(record.lifecycle.as_str(), "starting" | "launch_failed")
        {
            // The pane is the one agentctl created; its shell exec'd into the slot relay
            // agentctl ran. A relay still running there with the slot as its cwd is that
            // launch, pinned or not.
            let info = self.client.pane_info(pane)?;
            let cwd_matches = info.cwd == record.cwd
                || fs::canonicalize(&info.cwd)
                    .ok()
                    .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok());
            if info.pane_id == pane
                && Some(&info.workspace_id) == record.workspace_id.as_ref()
                && cwd_matches
            {
                if let Some(observed) = self.client.relay_process(pane)? {
                    if record
                        .custom_process_identity
                        .as_ref()
                        .is_none_or(|expected| *expected == observed)
                    {
                        return Ok(());
                    }
                }
            }
            self.checked(record)?;
            return Ok(());
        }
        if record.adapter != "herdr-pane" {
            if record.lifecycle != "launch_failed" || self.client.pane_info(pane)?.agent.is_some() {
                self.checked(record)?;
            }
            return Ok(());
        }
        if matches!(record.lifecycle.as_str(), "starting" | "launch_failed")
            && record.custom_process_identity.is_some()
        {
            let info = self.client.pane_info(pane)?;
            let cwd_matches = info.cwd == record.cwd
                || fs::canonicalize(&info.cwd)
                    .ok()
                    .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok());
            if info.pane_id == pane
                && Some(&info.workspace_id) == record.workspace_id.as_ref()
                && cwd_matches
                && self
                    .client
                    .verify_custom_harness(
                        pane,
                        &record.harness,
                        record.custom_process_identity.as_ref(),
                    )
                    .is_ok()
            {
                return Ok(());
            }
        }
        if record.lifecycle == "launch_failed" && record.pane_reported_by_agentctl {
            let info = self.client.pane_info(pane)?;
            let cwd_matches = info.cwd == record.cwd
                || fs::canonicalize(&info.cwd)
                    .ok()
                    .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok());
            if info.pane_id == pane
                && Some(&info.workspace_id) == record.workspace_id.as_ref()
                && cwd_matches
                && info.agent.as_deref() == Some(&record.harness)
                && self.client.pane_is_idle_shell(pane)?
            {
                return Ok(());
            }
        }
        if record.lifecycle == "starting" && record.custom_process_identity.is_none() {
            return Err(fail(format!(
                "cannot prove starting custom harness ownership in pane {pane}"
            )));
        }
        self.checked(record)?;
        Ok(())
    }

    fn dead_pane_snapshot(
        &self,
        record: &AgentRecord,
        operation: &str,
    ) -> Result<(Pane, AgentPaneInfo)> {
        let pane_id = record.pane_id.as_deref().ok_or_else(|| {
            fail(format!(
                "refusing to {operation} {:?}: record lacks pane identity",
                record.name
            ))
        })?;
        let tab_id = record.tab_id.as_deref().ok_or_else(|| {
            fail(format!(
                "refusing to {operation} {:?}: record lacks tab identity",
                record.name
            ))
        })?;
        let workspace_id = record.workspace_id.as_deref().ok_or_else(|| {
            fail(format!(
                "refusing to {operation} {:?}: record lacks workspace identity",
                record.name
            ))
        })?;
        let panes = self.client.panes()?;
        let mut presentations: Vec<Pane> = panes
            .iter()
            .filter(|pane| pane.pane_id == pane_id)
            .cloned()
            .collect();
        if presentations.len() != 1 {
            return Err(fail(format!(
                "refusing to {operation} {:?}: expected one recorded pane, found {}",
                record.name,
                presentations.len()
            )));
        }
        let presentation = presentations.pop().expect("one presentation was checked");
        if presentation.tab_id != tab_id || presentation.workspace_id != workspace_id {
            return Err(fail(format!(
                "refusing to {operation} {:?}: recorded pane, tab, or workspace changed",
                record.name
            )));
        }
        let tab_panes: Vec<&Pane> = panes.iter().filter(|pane| pane.tab_id == tab_id).collect();
        if tab_panes.len() != 1 || *tab_panes[0] != presentation {
            return Err(fail(format!(
                "refusing to {operation} {:?}: recorded tab is not the exact one-pane tab",
                record.name
            )));
        }
        let info = self.client.pane_info(pane_id)?;
        let cwd_matches = info.cwd == record.cwd
            || fs::canonicalize(&info.cwd)
                .ok()
                .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok());
        if info.pane_id != pane_id || info.workspace_id != workspace_id || !cwd_matches {
            return Err(fail(format!(
                "refusing to {operation} {:?}: recorded pane, workspace, or cwd changed",
                record.name
            )));
        }
        if let Some(agent) = info.agent.as_deref() {
            return Err(fail(format!(
                "refusing to {operation} {:?}: pane still reports agent {agent:?}",
                record.name
            )));
        }
        if info.session_agent.is_some() || info.session_value.is_some() {
            return Err(fail(format!(
                "refusing to {operation} {:?}: absent agent has native session identity",
                record.name
            )));
        }
        Ok((presentation, info))
    }

    fn dead_pane_proof(&self, record: &AgentRecord, operation: &str) -> Result<DeadPaneProof> {
        let pane_id = record.pane_id.as_deref().ok_or_else(|| {
            fail(format!(
                "refusing to {operation} {:?}: record lacks pane identity",
                record.name
            ))
        })?;
        let (presentation, info) = self.dead_pane_snapshot(record, operation)?;
        let shell = self
            .client
            .pane_idle_shell_identity(pane_id)
            .map_err(|error| {
                fail(format!(
                    "refusing to {operation} {:?}: cannot prove supported idle shell generation: {error}",
                    record.name
                ))
            })?
            .ok_or_else(|| {
                fail(format!(
                    "refusing to {operation} {:?}: pane is not a supported idle shell generation without descendants",
                    record.name
                ))
            })?;
        let (final_presentation, final_info) = self.dead_pane_snapshot(record, operation)?;
        if final_presentation != presentation || final_info != info {
            return Err(fail(format!(
                "refusing to {operation} {:?}: pane membership or agent state changed during shell proof",
                record.name
            )));
        }
        Ok(DeadPaneProof {
            info: final_info,
            presentation: final_presentation,
            shell,
        })
    }

    fn bounded_terminal_text(&self, pane_id: &str) -> Result<String> {
        let mut text = self
            .client
            .read(pane_id, "recent-unwrapped", Some(5000))
            .map_err(|error| {
                fail(format!(
                    "cannot preserve terminal output before stop: {error}"
                ))
            })?;
        if text.is_empty() {
            text = self
                .client
                .read(pane_id, "recent", Some(5000))
                .map_err(|error| {
                    fail(format!(
                        "cannot preserve terminal output before stop: {error}"
                    ))
                })?;
        }
        Ok(text)
    }

    fn shell_proof_json(proof: &PaneShellProof) -> Value {
        json!({
            "version": proof.identity.version,
            "boot_id": proof.identity.boot_id,
            "pid": proof.identity.pid,
            "starttime_ticks": proof.identity.starttime_ticks,
            "executable_device": proof.identity.executable_device,
            "executable_inode": proof.identity.executable_inode,
            "executable_path": proof.executable_path,
        })
    }

    fn archive_destination(&self, record: &AgentRecord) -> Result<(PathBuf, PathBuf)> {
        let archive = self.registry.join("archive");
        agent::create_private_directory(&archive, "agent archive", false, false)?;
        let destination = archive.join(format!("{}-{}", record.name, record.token));
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(fail(format!(
                    "refusing to overwrite existing agent archive {}",
                    destination.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(fail(error.to_string())),
        }
        Ok((archive, destination))
    }

    fn publish_pinned_directory(
        &self,
        pinned: &PinnedAgentDirectory,
        destination: &Path,
        expected_record: &[u8],
    ) -> Result<()> {
        let mut published = false;
        self.publish_pinned_directory_with(
            pinned,
            destination,
            expected_record,
            &mut published,
            (
                rename_directory_noreplace_at,
                || {},
                || {},
                |directory: &File, _label| directory.sync_all(),
            ),
        )
    }

    fn publish_pinned_directory_with<Rename, AfterRename, AfterRollback, Sync>(
        &self,
        pinned: &PinnedAgentDirectory,
        destination: &Path,
        expected_record: &[u8],
        published: &mut bool,
        publication_hooks: (Rename, AfterRename, AfterRollback, Sync),
    ) -> Result<()>
    where
        Rename: FnOnce(&File, &str, &File, &str) -> Result<()>,
        AfterRename: FnOnce(),
        AfterRollback: FnOnce(),
        Sync: FnMut(&File, &str) -> io::Result<()>,
    {
        self.publish_pinned_directory_with_preflight(
            pinned,
            destination,
            expected_record,
            published,
            || Ok(()),
            publication_hooks,
        )
    }

    fn publish_pinned_directory_with_preflight<
        BeforeRename,
        Rename,
        AfterRename,
        AfterRollback,
        Sync,
    >(
        &self,
        pinned: &PinnedAgentDirectory,
        destination: &Path,
        expected_record: &[u8],
        published: &mut bool,
        before_rename: BeforeRename,
        publication_hooks: (Rename, AfterRename, AfterRollback, Sync),
    ) -> Result<()>
    where
        BeforeRename: FnOnce() -> Result<()>,
        Rename: FnOnce(&File, &str, &File, &str) -> Result<()>,
        AfterRename: FnOnce(),
        AfterRollback: FnOnce(),
        Sync: FnMut(&File, &str) -> io::Result<()>,
    {
        let (rename, after_rename, after_rollback, mut sync) = publication_hooks;
        *published = false;
        let archive_path = destination
            .parent()
            .ok_or_else(|| fail("agent archive destination has no parent"))?;
        if archive_path != self.registry.join("archive") {
            return Err(fail("invalid agent archive destination"));
        }
        let destination_name = destination
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| fail("invalid agent archive destination name"))?;
        let registry_parent = Self::pinned_parent_directory(&self.registry, "agent registry")?;
        let archive_parent = Self::pinned_parent_directory(archive_path, "agent archive")?;
        Self::verify_pinned_agent_directory(pinned)?;
        if Self::child_directory_identity(&registry_parent.file, &pinned.name)?
            != Some((pinned.device, pinned.inode))
        {
            return Err(fail(format!(
                "agent {:?} registry directory changed before publication",
                pinned.name
            )));
        }
        before_rename()?;
        if let Err(rename_error) = rename(
            &registry_parent.file,
            &pinned.name,
            &archive_parent.file,
            destination_name,
        ) {
            let active = Self::child_directory_identity(&registry_parent.file, &pinned.name);
            let archived = Self::child_directory_identity(&archive_parent.file, destination_name);
            let opened = pinned
                .file
                .metadata()
                .map_err(|error| fail(format!("cannot reinspect pinned agent directory: {error}")))
                .and_then(|metadata| {
                    Self::private_directory_identity(&metadata, "pinned agent directory")
                });
            match (active, archived, opened) {
                (Ok(Some(active)), _archived, Ok(opened))
                    if active == (pinned.device, pinned.inode)
                        && opened == (pinned.device, pinned.inode) =>
                {
                    return Err(rename_error);
                }
                (Ok(None), Ok(Some(archived)), Ok(opened))
                    if archived == (pinned.device, pinned.inode)
                        && opened == (pinned.device, pinned.inode) =>
                {
                    *published = true;
                    return match self.record_bytes_with(pinned, false) {
                        Ok(record) if record == expected_record => Err(fail(format!(
                            "archive rename reported failure ({rename_error}) after the proved generation was published"
                        ))),
                        Ok(_) => Err(fail(format!(
                            "archive rename reported failure ({rename_error}); the generation was published but its record changed"
                        ))),
                        Err(proof) => Err(fail(format!(
                            "archive rename reported failure ({rename_error}); the generation was published but its record could not be proved: {proof}"
                        ))),
                    };
                }
                (active, archived, opened) => {
                    *published = true;
                    return Err(fail(format!(
                        "archive rename reported failure ({rename_error}) with an ambiguous namespace outcome: active={active:?}, archive={archived:?}, opened={opened:?}"
                    )));
                }
            }
        }
        *published = true;
        after_rename();
        let result = (|| -> Result<()> {
            Self::verify_pinned_parent_directory(&registry_parent, "agent registry")?;
            Self::verify_pinned_parent_directory(&archive_parent, "agent archive")?;
            if Self::child_directory_identity(&registry_parent.file, &pinned.name)?.is_some() {
                return Err(fail(format!(
                    "agent {:?} active name reappeared during publication",
                    pinned.name
                )));
            }
            let published = Self::child_directory_identity(&archive_parent.file, destination_name)?
                .ok_or_else(|| fail("published agent directory disappeared"))?;
            let opened = pinned.file.metadata().map_err(|error| {
                fail(format!("cannot reinspect pinned agent directory: {error}"))
            })?;
            let opened_identity =
                Self::private_directory_identity(&opened, "pinned agent directory")?;
            if published != (pinned.device, pinned.inode)
                || opened_identity != (pinned.device, pinned.inode)
            {
                return Err(fail(format!(
                    "agent {:?} published directory was not the proved generation",
                    pinned.name
                )));
            }
            if self.record_bytes_with(pinned, false)? != expected_record {
                return Err(fail(format!(
                    "agent {:?} record changed during archival publication",
                    pinned.name
                )));
            }
            Ok(())
        })();
        if let Err(error) = result {
            // All agentctl participants hold the name lock. Reprove both
            // entries immediately before the inverse rename so a replacement
            // at either name is never promoted to active state. renameat2
            // cannot compare an inode atomically, so an actor that ignores the
            // cooperative lock can still race this last check; RENAME_NOREPLACE
            // at least refuses an occupied active name.
            let active = match Self::child_directory_identity(
                &registry_parent.file,
                &pinned.name,
            ) {
                Ok(value) => value,
                Err(proof) => {
                    return Err(fail(format!(
                        "archive identity check failed ({error}); cannot prove the active name absent before rollback: {proof}"
                    )))
                }
            };
            if active.is_some() {
                return Err(fail(format!(
                    "archive identity check failed ({error}); active name reappeared, so rollback was not attempted"
                )));
            }
            let rollback_source = match Self::child_directory_identity(
                &archive_parent.file,
                destination_name,
            ) {
                Ok(value) => value,
                Err(proof) => {
                    return Err(fail(format!(
                        "archive identity check failed ({error}); cannot reprove the published generation before rollback: {proof}"
                    )))
                }
            };
            if rollback_source != Some((pinned.device, pinned.inode)) {
                return Err(fail(format!(
                    "archive identity check failed ({error}); archive destination was replaced, so rollback was not attempted"
                )));
            }
            if let Err(rollback) = rename_directory_noreplace_at(
                &archive_parent.file,
                destination_name,
                &registry_parent.file,
                &pinned.name,
            ) {
                return Err(fail(format!(
                    "archive identity check failed ({error}) and rollback was incomplete: {rollback}"
                )));
            }
            after_rollback();
            let rollback_proof = (|| -> Result<()> {
                Self::verify_pinned_parent_directory(&registry_parent, "agent registry")?;
                Self::verify_pinned_parent_directory(&archive_parent, "agent archive")?;
                Self::verify_pinned_agent_directory(pinned)?;
                if Self::child_directory_identity(&registry_parent.file, &pinned.name)?
                    != Some((pinned.device, pinned.inode))
                {
                    return Err(fail("restored active name is not the proved generation"));
                }
                if Self::child_directory_identity(&archive_parent.file, destination_name)?.is_some()
                {
                    return Err(fail("archive destination remained after rollback"));
                }
                if self.record_bytes(pinned)? != expected_record {
                    return Err(fail("restored agent record is not the proved content"));
                }
                Ok(())
            })();
            if let Err(rollback_proof) = rollback_proof {
                return Err(fail(format!(
                    "archive identity check failed ({error}); inverse rename completed but rollback state could not be proved: {rollback_proof}"
                )));
            }
            let mut failures = Vec::new();
            for (directory, label) in [
                (&archive_parent.file, "agent archive rollback"),
                (&registry_parent.file, "agent registry rollback"),
            ] {
                if let Err(sync_error) = sync(directory, label) {
                    failures.push(format!("{label}: {sync_error}"));
                }
            }
            if !failures.is_empty() {
                return Err(fail(format!(
                    "archive identity check failed ({error}); rollback completed but directory durability is uncertain: {}",
                    failures.join("; ")
                )));
            }
            *published = false;
            return Err(error);
        }
        let mut failures = Vec::new();
        for (directory, label) in [
            (&archive_parent.file, "published agent archive"),
            (&registry_parent.file, "published agent registry"),
        ] {
            if let Err(sync_error) = sync(directory, label) {
                failures.push(format!("{label}: {sync_error}"));
            }
        }
        if !failures.is_empty() {
            return Err(fail(format!(
                "agent archive was published but directory durability is uncertain: {}",
                failures.join("; ")
            )));
        }
        Ok(())
    }

    fn unlink_pinned_file(pinned: &PinnedAgentDirectory, name: &str) -> Result<()> {
        let name = CString::new(name).map_err(|_| fail("registry artifact name contains NUL"))?;
        let result = unsafe { libc::unlinkat(pinned.file.as_raw_fd(), name.as_ptr(), 0) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(fail(format!("cannot remove recovery snapshot: {error}")));
            }
        }
        pinned
            .file
            .sync_all()
            .map_err(|error| fail(format!("cannot sync restored agent directory: {error}")))
    }

    fn restore_output_snapshot(
        pinned: &PinnedAgentDirectory,
        previous: Option<&[u8]>,
    ) -> Result<()> {
        match previous {
            Some(content) => atomic_replace_bytes(pinned, "output.json", content)
                .map(|_| ())
                .map_err(|error| *error.error),
            None => Self::unlink_pinned_file(pinned, "output.json"),
        }
    }

    fn verify_installed_output_snapshot(
        pinned: &PinnedAgentDirectory,
        installed: InstalledArtifact,
    ) -> Result<()> {
        let mut current = Self::open_pinned_file(pinned, "output.json", libc::O_RDONLY)?;
        let before = current
            .metadata()
            .map_err(|error| fail(format!("cannot reprove installed output: {error}")))?;
        let uid = unsafe { libc::getuid() };
        let mut content = Vec::with_capacity(installed.size as usize);
        Read::by_ref(&mut current)
            .take(installed.size + 1)
            .read_to_end(&mut content)
            .map_err(|error| fail(format!("cannot reread installed output: {error}")))?;
        let after = current
            .metadata()
            .map_err(|error| fail(format!("cannot reinspect installed output: {error}")))?;
        if !before.is_file()
            || before.uid() != uid
            || before.permissions().mode() & 0o077 != 0
            || before.nlink() != 1
            || before.len() != installed.size
            || content.len() as u64 != installed.size
            || <[u8; 32]>::from(Sha256::digest(&content)) != installed.digest
            || (before.dev(), before.ino()) != (installed.device, installed.inode)
            || after.dev() != before.dev()
            || after.ino() != before.ino()
            || after.mode() != before.mode()
            || after.uid() != before.uid()
            || after.nlink() != before.nlink()
            || after.len() != before.len()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
        {
            return Err(fail(
                "installed output generation changed before use; replacement was preserved",
            ));
        }
        Ok(())
    }

    fn restore_installed_output_snapshot(
        pinned: &PinnedAgentDirectory,
        previous: Option<&[u8]>,
        installed: InstalledArtifact,
    ) -> Result<()> {
        Self::verify_installed_output_snapshot(pinned, installed)?;
        // Registry participants hold the per-name lock; a same-uid process
        // ignoring it can still race this final proof and replacement.
        Self::restore_output_snapshot(pinned, previous)
    }

    fn optional_snapshot_bytes(pinned: &PinnedAgentDirectory) -> Result<Option<Vec<u8>>> {
        let path = pinned.path.join("output.json");
        let Some(mut file) =
            Self::open_optional_pinned_file(pinned, "output.json", libc::O_RDONLY)?
        else {
            return Ok(None);
        };
        let before = file
            .metadata()
            .map_err(|error| fail(format!("cannot inspect output snapshot: {error}")))?;
        let uid = unsafe { libc::getuid() };
        if !before.is_file()
            || before.uid() != uid
            || before.permissions().mode() & 0o077 != 0
            || before.nlink() != 1
            || before.len() > MAX_SNAPSHOT_BYTES as u64
        {
            return Err(fail(format!(
                "unsafe existing output snapshot: {}",
                path.display()
            )));
        }
        let mut content = Vec::with_capacity(before.len() as usize);
        Read::by_ref(&mut file)
            .take((MAX_SNAPSHOT_BYTES + 1) as u64)
            .read_to_end(&mut content)
            .map_err(|error| fail(format!("cannot read output snapshot: {error}")))?;
        let after = file
            .metadata()
            .map_err(|error| fail(format!("cannot reinspect output snapshot: {error}")))?;
        if content.len() > MAX_SNAPSHOT_BYTES
            || before.len() != content.len() as u64
            || after.dev() != before.dev()
            || after.ino() != before.ino()
            || after.mode() != before.mode()
            || after.uid() != before.uid()
            || after.nlink() != before.nlink()
            || after.len() != before.len()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
        {
            return Err(fail(format!(
                "existing output snapshot changed while reading: {}",
                path.display()
            )));
        }
        Ok(Some(content))
    }

    fn publish_archive(
        &self,
        pinned: &PinnedAgentDirectory,
        destination: &Path,
        snapshot: &Value,
        expected_record: &[u8],
        before_publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.publish_archive_with(
            pinned,
            destination,
            snapshot,
            expected_record,
            before_publish,
            (
                |_pinned, _temporary| Ok(()),
                |_pinned, _name, _temporary| Ok(()),
                rename_directory_noreplace_at,
                || {},
                || {},
                |directory: &File, _label| directory.sync_all(),
            ),
        )
    }

    fn publish_archive_with<AfterWrite, AfterInstall, Rename, AfterRename, AfterRollback, Sync>(
        &self,
        pinned: &PinnedAgentDirectory,
        destination: &Path,
        snapshot: &Value,
        expected_record: &[u8],
        before_publish: impl FnOnce() -> Result<()>,
        publication_hooks: (
            AfterWrite,
            AfterInstall,
            Rename,
            AfterRename,
            AfterRollback,
            Sync,
        ),
    ) -> Result<()>
    where
        AfterWrite: FnOnce(&PinnedAgentDirectory, &str) -> Result<()>,
        AfterInstall: FnOnce(&PinnedAgentDirectory, &str, &str) -> Result<()>,
        Rename: FnOnce(&File, &str, &File, &str) -> Result<()>,
        AfterRename: FnOnce(),
        AfterRollback: FnOnce(),
        Sync: FnMut(&File, &str) -> io::Result<()>,
    {
        let (after_write, after_install, rename, after_rename, after_rollback, sync) =
            publication_hooks;
        let previous = Self::optional_snapshot_bytes(pinned)?;
        let encoded = serde_json::to_vec_pretty(snapshot)
            .map_err(|error| fail(format!("cannot serialize output snapshot: {error}")))?;
        let mut encoded = encoded;
        encoded.push(b'\n');
        let installed = match atomic_replace_bytes_with(
            pinned,
            "output.json",
            &encoded,
            (after_write, after_install),
        ) {
            Ok(installed) => installed,
            Err(error) => match error.state {
                ArtifactInstallState::NotInstalled => return Err(*error.error),
                ArtifactInstallState::Uncertain => {
                    return Err(fail(format!(
                        "archive output installation is uncertain; replacement was preserved: {}",
                        error.error
                    )))
                }
                ArtifactInstallState::Installed(installed) => {
                    if let Err(rollback) = Self::restore_installed_output_snapshot(
                        pinned,
                        previous.as_deref(),
                        installed,
                    ) {
                        return Err(fail(format!(
                            "archive preparation failed ({}) and exact output rollback was unsafe or incomplete: {rollback}",
                            error.error
                        )));
                    }
                    return Err(*error.error);
                }
            },
        };
        if let Err(error) = Self::verify_installed_output_snapshot(pinned, installed) {
            return Err(fail(format!(
                "archive output changed after preparation; replacement was preserved: {error}"
            )));
        }
        if let Err(error) = before_publish() {
            if let Err(rollback) =
                Self::restore_installed_output_snapshot(pinned, previous.as_deref(), installed)
            {
                return Err(fail(format!(
                    "archive preparation failed ({error}) and exact output rollback was unsafe or incomplete: {rollback}"
                )));
            }
            return Err(error);
        }
        let mut published = false;
        if let Err(error) = self.publish_pinned_directory_with(
            pinned,
            destination,
            expected_record,
            &mut published,
            (rename, after_rename, after_rollback, sync),
        ) {
            if published {
                return Err(error);
            }
            let rollback =
                Self::restore_installed_output_snapshot(pinned, previous.as_deref(), installed);
            if let Err(rollback) = rollback {
                return Err(fail(format!(
                    "archive publication failed ({error}) and output rollback was incomplete: {rollback}"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    fn recover_legacy_adoption_locked(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        options: &StopOptions,
    ) -> Result<Value> {
        self.recover_legacy_adoption_locked_with(record, pinned, options, || {})
    }

    fn recover_legacy_adoption_locked_with(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        options: &StopOptions,
        after_output_preparation: impl FnOnce(),
    ) -> Result<Value> {
        let expected_token = options.expected_token.as_deref().ok_or_else(|| {
            fail("legacy adoption recovery requires --expected-token and --expected-record-sha256")
        })?;
        let expected_hash = options.expected_record_sha256.as_deref().ok_or_else(|| {
            fail("legacy adoption recovery requires --expected-token and --expected-record-sha256")
        })?;
        let initial = self.legacy_record_snapshot(pinned, expected_token, expected_hash)?;
        if &initial.record != record {
            return Err(fail(format!(
                "refusing to recover adoption {:?}: registry record changed",
                record.name
            )));
        }
        let record = &initial.record;
        if record.adapter != "herdr-foreign"
            || record.lifecycle != "running"
            || record.mode != "interactive"
            || record.backend != "herdr"
            || record.foreign_shell_identity.is_some()
        {
            return Err(fail(
                "--recover-legacy-adoption applies only to a running herdr-foreign record missing foreign_shell_identity",
            ));
        }
        let before = self.dead_pane_proof(record, "recover legacy adoption")?;
        let text = self.bounded_terminal_text(&before.info.pane_id)?;
        let current_snapshot =
            self.legacy_record_snapshot(pinned, expected_token, expected_hash)?;
        if !current_snapshot.same_generation(&initial) {
            return Err(fail(format!(
                "refusing to recover legacy adoption {:?}: registry record changed",
                record.name
            )));
        }
        let current = &current_snapshot.record;
        let after = self.dead_pane_proof(current, "recover legacy adoption")?;
        if after != before {
            return Err(fail(format!(
                "refusing to recover legacy adoption {:?}: runtime identity changed during output capture",
                record.name
            )));
        }
        let (_archive, destination) = self.archive_destination(current)?;
        let captured = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?
            .as_secs_f64();
        self.publish_archive(
            pinned,
            &destination,
            &json!({
                "text": text,
                "captured_at": captured,
                "pane_id": current.pane_id,
                "recovery_shell_identity": Self::shell_proof_json(&before.shell),
            }),
            &initial.content,
            || {
                after_output_preparation();
                let final_snapshot =
                    self.legacy_record_snapshot(pinned, expected_token, expected_hash)?;
                if !final_snapshot.same_generation(&initial) {
                    return Err(fail(format!(
                        "refusing to recover legacy adoption {:?}: registry record changed",
                        record.name
                    )));
                }
                if self.dead_pane_proof(&final_snapshot.record, "recover legacy adoption")?
                    != before
                {
                    return Err(fail(format!(
                        "refusing to recover legacy adoption {:?}: runtime identity changed before archival",
                        record.name
                    )));
                }
                Ok(())
            },
        )?;
        Ok(json!({
            "name": record.name,
            "archive": destination,
            "pane_closed": false,
            "tab_closed": false,
            "runtime_preserved": true,
            "recovered_legacy_adoption": true,
            "record_sha256": expected_hash,
            "recovery_shell_identity": Self::shell_proof_json(&before.shell),
        }))
    }

    fn require_dead_adoption(&self, record: &AgentRecord) -> Result<()> {
        let anchor = Self::dead_adoption_anchor(record)?;
        match self.client.process_liveness(anchor) {
            ProcessLiveness::Dead => Ok(()),
            ProcessLiveness::Alive => Err(fail(
                "the recorded adopted harness generation is still alive",
            )),
            ProcessLiveness::Unknown => Err(fail(
                "cannot prove the recorded adopted harness generation is dead",
            )),
        }
    }

    fn dead_adoption_stop_recovery(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        queue: &RetirementQueue,
    ) -> Result<Option<RecoveryAction>> {
        let Ok(anchor) = Self::dead_adoption_anchor(record) else {
            return Ok(None);
        };
        let initial = self.dead_adoption_snapshot(pinned, &record.token)?;
        if &initial.record != record {
            return Err(fail("adopted record changed before stop recovery advice"));
        }
        if self.client.process_liveness(anchor) != ProcessLiveness::Dead {
            return Ok(None);
        }
        self.verify_retirement_queue(pinned, queue)?;
        if self.dead_adoption_snapshot(pinned, &record.token)? != initial {
            return Err(fail("adopted record changed during stop recovery advice"));
        }
        if self.client.process_liveness(anchor) != ProcessLiveness::Dead {
            return Ok(None);
        }
        self.verify_retirement_queue(pinned, queue)?;
        if self.dead_adoption_snapshot(pinned, &record.token)? != initial {
            return Err(fail(
                "adopted record changed during final stop recovery proof",
            ));
        }
        Ok(Some(RecoveryAction::RetireDeadAdoption {
            name: record.name.clone(),
            token: record.token.clone(),
            record_sha256: format!("{:x}", Sha256::digest(&initial.content)),
        }))
    }

    fn retire_dead_adoption_locked(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        queue: &RetirementQueue,
        options: &StopOptions,
    ) -> Result<Value> {
        self.retire_dead_adoption_locked_with(record, pinned, queue, options, || {})
    }

    fn retire_dead_adoption_locked_with(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        queue: &RetirementQueue,
        options: &StopOptions,
        after_initial_proof: impl FnOnce(),
    ) -> Result<Value> {
        let expected_token = options.expected_token.as_deref().ok_or_else(|| {
            fail("dead adoption retirement requires --expected-token and --expected-record-sha256")
        })?;
        let expected_hash = options.expected_record_sha256.as_deref().ok_or_else(|| {
            fail("dead adoption retirement requires --expected-token and --expected-record-sha256")
        })?;
        if !stop_advice::valid_record_digest(expected_hash) {
            return Err(fail(
                "expected-record-sha256 must be exactly 64 lowercase hexadecimal characters",
            ));
        }
        let initial = self.dead_adoption_snapshot(pinned, expected_token)?;
        if &initial.record != record
            || format!("{:x}", Sha256::digest(&initial.content)) != expected_hash
        {
            return Err(fail(
                "adopted record changed before dead adoption retirement",
            ));
        }
        self.verify_retirement_queue(pinned, queue)?;
        self.require_dead_adoption(&initial.record)?;
        after_initial_proof();
        let (_, destination) = self.archive_destination(&initial.record)?;
        let mut published = false;
        self.publish_pinned_directory_with_preflight(
            pinned,
            &destination,
            &initial.content,
            &mut published,
            || {
                self.refuse_pending_rename(&[&record.name])?;
                if self.pending_move_destination_for_stop(record)?.is_some() {
                    return Err(fail(
                        "refusing dead adoption retirement: move is incomplete",
                    ));
                }
                self.verify_retirement_queue(pinned, queue)?;
                let final_snapshot = self.dead_adoption_snapshot(pinned, expected_token)?;
                if final_snapshot != initial {
                    return Err(fail(
                        "adopted record generation changed before dead adoption retirement",
                    ));
                }
                self.require_dead_adoption(&final_snapshot.record)?;
                self.verify_retirement_queue(pinned, queue)?;
                if self.dead_adoption_snapshot(pinned, expected_token)? != initial {
                    return Err(fail(
                        "adopted record generation changed during final death proof",
                    ));
                }
                Ok(())
            },
            (
                rename_directory_noreplace_at,
                || {},
                || {},
                |directory: &File, _label| directory.sync_all(),
            ),
        )?;
        Ok(json!({
            "name": record.name,
            "archive": destination,
            "pane_closed": false,
            "tab_closed": false,
            "runtime_preserved": true,
            "retired_dead_adoption": true,
            "record_sha256": expected_hash,
        }))
    }

    fn retire_managed_dead_locked(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        options: &StopOptions,
        recovery: &mut RecoveryAction,
    ) -> Result<Value> {
        if options.expected_token.is_none() {
            let snapshot = self.managed_record_snapshot(pinned, &record.token)?;
            if &snapshot.record != record {
                return Err(fail(format!(
                    "agent {:?} record changed before stop recovery advice",
                    record.name
                )));
            }
            *recovery = RecoveryAction::Stop {
                name: record.name.clone(),
                token: record.token.clone(),
                record_sha256: None,
                skip_cloud_halt: options.skip_cloud_halt,
            };
        }
        self.retire_managed_dead_locked_with(
            record,
            pinned,
            options.expected_token.as_deref(),
            || {},
            || {},
            (
                |_pinned, _temporary| Ok(()),
                |_pinned, _name, _temporary| Ok(()),
            ),
        )
    }

    fn retire_managed_dead_locked_with<AfterWrite, AfterInstall>(
        &self,
        record: &AgentRecord,
        pinned: &PinnedAgentDirectory,
        expected_token: Option<&str>,
        after_preparation: impl FnOnce(),
        after_final_record_proof: impl FnOnce(),
        artifact_hooks: (AfterWrite, AfterInstall),
    ) -> Result<Value>
    where
        AfterWrite: FnOnce(&PinnedAgentDirectory, &str) -> Result<()>,
        AfterInstall: FnOnce(&PinnedAgentDirectory, &str, &str) -> Result<()>,
    {
        let expected_token = expected_token.ok_or_else(|| {
            fail(format!(
                "retiring dead managed agent {:?} requires --expected-token",
                record.name
            ))
        })?;
        let initial = self.managed_record_snapshot(pinned, expected_token)?;
        if &initial.record != record {
            return Err(fail(format!(
                "refusing to retire dead managed agent {:?}: registry record changed",
                record.name
            )));
        }
        let record = &initial.record;
        if record.adapter != "herdr"
            || record.lifecycle != "running"
            || record.mode != "interactive"
            || record.backend != "herdr"
        {
            return Err(fail(
                "managed-dead retirement requires a running herdr record in interactive/herdr mode",
            ));
        }
        let before = self.dead_pane_proof(record, "retire dead managed agent")?;
        let text = self.bounded_terminal_text(&before.info.pane_id)?;
        let current_snapshot = self.managed_record_snapshot(pinned, expected_token)?;
        if !current_snapshot.same_generation(&initial) {
            return Err(fail(format!(
                "refusing to retire dead managed agent {:?}: registry record changed",
                record.name
            )));
        }
        let current = &current_snapshot.record;
        if self.dead_pane_proof(current, "retire dead managed agent")? != before {
            return Err(fail(format!(
                "refusing to retire dead managed agent {:?}: runtime identity changed during output capture",
                record.name
            )));
        }
        let (_archive, destination) = self.archive_destination(current)?;
        let final_snapshot = self.managed_record_snapshot(pinned, expected_token)?;
        if !final_snapshot.same_generation(&initial) {
            return Err(fail(format!(
                "refusing to retire dead managed agent {:?}: registry record changed",
                record.name
            )));
        }
        if self.dead_pane_proof(&final_snapshot.record, "retire dead managed agent")? != before {
            return Err(fail(format!(
                "refusing to retire dead managed agent {:?}: runtime identity changed before close",
                record.name
            )));
        }
        let mut final_record = final_snapshot.record;
        final_record.lifecycle = "stopped".to_owned();
        let mut stopped_bytes = final_record.document_pretty_bytes()?;
        stopped_bytes.push(b'\n');
        if stopped_bytes.len() > MAX_AGENT_RECORD_BYTES {
            return Err(fail(format!(
                "refusing stopped agent record larger than {MAX_AGENT_RECORD_BYTES} bytes"
            )));
        }
        let captured = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| fail(error.to_string()))?
            .as_secs_f64();
        let previous_output = Self::optional_snapshot_bytes(pinned)?;
        let snapshot = json!({
            "text": text,
            "captured_at": captured,
            "pane_id": current.pane_id,
            "retirement_shell_identity": Self::shell_proof_json(&before.shell),
        });
        let mut snapshot_bytes = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| fail(format!("cannot serialize output snapshot: {error}")))?;
        snapshot_bytes.push(b'\n');
        let installed = match atomic_replace_bytes_with(
            pinned,
            "output.json",
            &snapshot_bytes,
            artifact_hooks,
        ) {
            Ok(installed) => installed,
            Err(error) => match error.state {
                ArtifactInstallState::NotInstalled => return Err(*error.error),
                ArtifactInstallState::Uncertain => {
                    return Err(fail(format!(
                        "managed retirement output installation is uncertain; replacement was preserved: {}",
                        error.error
                    )))
                }
                ArtifactInstallState::Installed(installed) => {
                    if let Err(rollback) = Self::restore_installed_output_snapshot(
                        pinned,
                        previous_output.as_deref(),
                        installed,
                    ) {
                        return Err(fail(format!(
                            "managed retirement preparation failed ({}) and exact output rollback was unsafe or incomplete: {rollback}",
                            error.error
                        )));
                    }
                    return Err(*error.error);
                }
            },
        };
        after_preparation();
        let final_runtime_proof = (|| -> Result<()> {
            Self::verify_installed_output_snapshot(pinned, installed)?;
            if self.record_bytes(pinned)? != initial.content {
                return Err(fail(format!(
                    "refusing to retire dead managed agent {:?}: registry record changed immediately before close",
                    record.name
                )));
            }
            after_final_record_proof();
            self.dead_pane_proof(&final_record, "retire dead managed agent")
                .and_then(|proof| {
                if proof == before {
                    Ok(())
                } else {
                    Err(fail(format!(
                        "refusing to retire dead managed agent {:?}: runtime identity changed immediately before close",
                        record.name
                    )))
                }
            })?;
            Ok(())
        })();
        if let Err(error) = final_runtime_proof {
            let rollback = Self::restore_installed_output_snapshot(
                pinned,
                previous_output.as_deref(),
                installed,
            );
            if let Err(rollback) = rollback {
                return Err(fail(format!(
                    "managed retirement final runtime proof failed ({error}) and rollback was incomplete: {rollback}"
                )));
            }
            return Err(error);
        }
        let pane_id = final_record
            .pane_id
            .as_deref()
            .expect("proved pane identity");
        self.client.close_pane(pane_id)?;
        atomic_replace_bytes(pinned, "agent.json", &stopped_bytes).map_err(|error| *error.error)?;
        self.publish_pinned_directory(pinned, &destination, &stopped_bytes)?;
        let tab_closed = self.client.panes().ok().map(|panes| {
            panes
                .iter()
                .all(|pane| Some(&pane.tab_id) != final_record.tab_id.as_ref())
        });
        Ok(json!({
            "name": record.name,
            "archive": destination,
            "pane_closed": true,
            "tab_closed": tab_closed,
            "managed_dead": true,
        }))
    }

    /// Close an owned pane, or only unregister a foreign runtime, then archive state.
    pub fn stop(&self, agent_name: &str) -> Result<Value> {
        self.stop_with_options(agent_name, StopOptions::default())
    }

    /// Stop with explicit generation assertions or the loud adoption-recovery gate.
    pub fn stop_with_options(&self, agent_name: &str, options: StopOptions) -> Result<Value> {
        self.advised_stop_with_options(agent_name, options)
            .map_err(|failure| *failure.error)
    }

    /// Stop with recovery advice captured at the trusted refusal site.
    pub fn advised_stop_with_options(
        &self,
        agent_name: &str,
        options: StopOptions,
    ) -> std::result::Result<Value, StopFailure> {
        let mut recovery = RecoveryAction::Doctor;
        self.stop_with_options_inner(agent_name, &options, &mut recovery)
            .map_err(|error| StopFailure {
                error: Box::new(error),
                recovery: recovery.for_assertions(&options),
            })
    }

    fn stop_with_options_inner(
        &self,
        agent_name: &str,
        options: &StopOptions,
        recovery: &mut RecoveryAction,
    ) -> Result<Value> {
        let _lock = self.lock(agent_name)?;
        let mut record = self.load_with_stop_advice(agent_name, Some(&mut *recovery))?;
        if options.recover_legacy_adoption && options.retire_dead_adoption {
            return Err(fail(
                "--retire-dead-adoption and --recover-legacy-adoption are mutually exclusive",
            ));
        }
        if record.adapter == turn_runner::ADAPTER {
            return self.retire_turn_runner(record, options, recovery);
        }
        if options.retire_dead_adoption {
            Self::dead_adoption_anchor(&record)?;
        }
        if !record.is_cloud() {
            record.supported()?;
            if options.skip_cloud_halt {
                return Err(fail("--skip-cloud-halt applies only to agentcloud agents"));
            }
        }
        if options
            .expected_token
            .as_deref()
            .is_some_and(|expected| expected != record.token)
        {
            return Err(fail(format!(
                "agent {agent_name:?} was replaced before this operation"
            )));
        }
        let identity_lock = if options.retire_dead_adoption {
            Some(self.identity_lock()?)
        } else if !options.recover_legacy_adoption && Self::dead_adoption_anchor(&record).is_ok() {
            self.identity_lock().ok()
        } else {
            None
        };
        let confirmed_record = self.load_with_stop_advice(agent_name, Some(&mut *recovery))?;
        if confirmed_record.token != record.token
            || confirmed_record.document()? != record.document()?
        {
            return Err(fail(format!(
                "agent {agent_name:?} record changed before stop"
            )));
        }
        record = confirmed_record;
        if record.is_cloud() {
            return self.cloud_stop(record, options);
        }
        if self.pending_move_destination_for_stop(&record)?.is_some() {
            *recovery = RecoveryAction::Move {
                name: record.name.clone(),
                token: record.token.clone(),
            };
            return Err(fail(format!(
                "refusing to stop {agent_name:?}: move is incomplete"
            )));
        }
        let retirement = if options.retire_dead_adoption {
            let pinned = self.pinned_agent_directory(agent_name)?;
            let queue = self.retirement_queue_locks(&pinned)?;
            Some((pinned, queue))
        } else if identity_lock.is_some() {
            self.pinned_agent_directory(agent_name)
                .and_then(|pinned| {
                    self.retirement_queue_locks(&pinned)
                        .map(|queue| (pinned, queue))
                })
                .ok()
        } else {
            None
        };
        if options.retire_dead_adoption {
            let (pinned, queue) = retirement
                .as_ref()
                .expect("retirement pins and locks are held");
            return self.retire_dead_adoption_locked(&record, pinned, queue, options);
        }
        if options.recover_legacy_adoption {
            let pane_id = record
                .pane_id
                .as_deref()
                .ok_or_else(|| fail("legacy adopted record has no pane identity"))?;
            let _pane_lock = self.pane_lock(pane_id)?;
            let pinned = self.pinned_agent_directory(agent_name)?;
            return self.recover_legacy_adoption_locked(&record, &pinned, options);
        }
        if options.expected_record_sha256.is_some() {
            return Err(fail(
                "--expected-record-sha256 requires --recover-legacy-adoption or --retire-dead-adoption",
            ));
        }
        if record.adapter == "herdr-foreign" {
            if let Some((pinned, queue)) = retirement.as_ref() {
                if let Some(action) = self
                    .dead_adoption_stop_recovery(&record, pinned, queue)
                    .ok()
                    .flatten()
                {
                    *recovery = action;
                }
            }
            if let Some(pane_id) = record.pane_id.as_deref() {
                if !self
                    .client
                    .panes()?
                    .iter()
                    .any(|pane| pane.pane_id == pane_id)
                {
                    if let Some((missing, detail)) = self.closed_foreign_target(&record) {
                        return self.archive_closed_foreign(agent_name, record, missing, &detail);
                    }
                }
            }
            let (live, info, presentation) =
                self.inspect_foreign_with_stop_advice(agent_name, &record, Some(&mut *recovery))?;
            let text = self.bounded_terminal_text(&info.pane_id)?;
            // Output capture is another control round trip. Refuse archival if
            // the exact foreign identity changed while it was in progress.
            let (final_live, final_info, final_presentation) = self
                .inspect_foreign(agent_name, &record)
                .map_err(|error| {
                    fail(format!(
                        "refusing to unregister adopted agent {agent_name:?}: runtime identity could not be reverified after output capture: {error}"
                    ))
                })?;
            let cwd_stable = final_info.cwd == info.cwd
                || fs::canonicalize(&final_info.cwd)
                    .ok()
                    .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&info.cwd).ok());
            if final_live != live
                || final_info.pane_id != info.pane_id
                || final_info.workspace_id != info.workspace_id
                || !cwd_stable
                || final_info.agent != info.agent
                || final_info.session_agent != info.session_agent
                || final_info.session_value != info.session_value
                || final_presentation != presentation
            {
                return Err(fail(format!(
                    "refusing to unregister adopted agent {agent_name:?}: runtime identity changed during output capture"
                )));
            }
            let (archive, destination) = self.archive_destination(&record)?;
            self.snapshot(&record, &text)?;
            let (persisted_live, persisted_info, persisted_presentation) = self
                .inspect_foreign(agent_name, &record)
                .map_err(|error| {
                    fail(format!(
                        "refusing to unregister adopted agent {agent_name:?}: runtime identity could not be reverified before archival: {error}"
                    ))
                })?;
            let persisted_cwd_stable = persisted_info.cwd == info.cwd
                || fs::canonicalize(&persisted_info.cwd)
                    .ok()
                    .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&info.cwd).ok());
            if persisted_live != live
                || persisted_info.pane_id != info.pane_id
                || persisted_info.workspace_id != info.workspace_id
                || !persisted_cwd_stable
                || persisted_info.agent != info.agent
                || persisted_info.session_agent != info.session_agent
                || persisted_info.session_value != info.session_value
                || persisted_presentation != presentation
            {
                return Err(fail(format!(
                    "refusing to unregister adopted agent {agent_name:?}: runtime identity changed before archival"
                )));
            }
            record.lifecycle = "stopped".to_owned();
            self.save(&record)?;
            fs::rename(self.directory(agent_name)?, &destination)
                .map_err(|error| fail(error.to_string()))?;
            agent::sync_directory(&archive)?;
            agent::sync_directory(&self.registry)?;
            return Ok(json!({
                "name": agent_name,
                "archive": destination,
                "pane_closed": false,
                "tab_closed": false,
                "runtime_preserved": true,
            }));
        }
        let panes = self.client.panes()?;
        if record.adapter == "herdr"
            && matches!(record.lifecycle.as_str(), "running" | "stopping")
            && record.pane_id.is_some()
        {
            let recorded: Vec<&Pane> = panes
                .iter()
                .filter(|pane| Some(&pane.pane_id) == record.pane_id.as_ref())
                .collect();
            if recorded.len() == 1 && self.client.pane_info(&recorded[0].pane_id)?.agent.is_none() {
                if record.lifecycle != "running" {
                    return Err(fail(
                        "managed-dead retirement requires a running herdr record",
                    ));
                }
                let _pane_lock = self.pane_lock(&recorded[0].pane_id)?;
                let pinned = self.pinned_agent_directory(agent_name)?;
                return self.retire_managed_dead_locked(&record, &pinned, options, recovery);
            }
        }
        if panes.iter().any(|pane| {
            Some(&pane.pane_id) == record.pane_id.as_ref()
                && Some(&pane.tab_id) != record.tab_id.as_ref()
        }) {
            return Err(fail(
                "refusing to archive an agent whose pane moved to another tab",
            ));
        }
        let owned: Vec<Pane> = panes
            .into_iter()
            .filter(|pane| Some(&pane.tab_id) == record.tab_id.as_ref())
            .collect();
        if record.pane_id.is_none() && record.lifecycle == "launch_failed" && owned.len() == 1 {
            let info = self.client.pane_info(&owned[0].pane_id)?;
            if info.agent.is_none()
                && Some(&info.workspace_id) == record.workspace_id.as_ref()
                && (info.cwd == record.cwd
                    || fs::canonicalize(&info.cwd)
                        .ok()
                        .is_some_and(|cwd| Some(cwd) == fs::canonicalize(&record.cwd).ok()))
            {
                record.pane_id = Some(owned[0].pane_id.clone());
                self.save(&record)?;
            }
        }
        let (archive, destination) = self.archive_destination(&record)?;
        if !owned.is_empty() {
            if owned.len() != 1
                || Some(&owned[0].pane_id) != record.pane_id.as_ref()
                || Some(&owned[0].workspace_id) != record.workspace_id.as_ref()
            {
                return Err(fail("refusing to close a tab whose pane ownership changed"));
            }
            if matches!(record.adapter.as_str(), "herdr-pane" | "herdr-relay")
                || record.lifecycle == "running"
                || self.client.pane_info(&owned[0].pane_id)?.agent.is_some()
            {
                self.checked_or_launch_failed(&record, &owned[0].pane_id)?;
            }
            let pane_id = &owned[0].pane_id;
            let mut text = self.client.read(pane_id, "recent-unwrapped", Some(5000))?;
            if text.is_empty() {
                text = self.client.read(pane_id, "recent", Some(5000))?;
            }
            self.snapshot(&record, &text)?;
            self.checked_or_launch_failed(&record, pane_id)?;
            record.lifecycle = "stopping".to_owned();
            self.save(&record)?;
            // A human can add a pane after the membership snapshot. Target the recorded
            // pane, never the whole tab; a newly added sibling must remain untouched.
            self.client.close_pane(pane_id)?;
        }
        record.lifecycle = "stopped".to_owned();
        self.save(&record)?;
        fs::rename(self.directory(agent_name)?, &destination)
            .map_err(|error| fail(error.to_string()))?;
        agent::sync_directory(&archive)?;
        agent::sync_directory(&self.registry)?;
        let tab_closed = if owned.is_empty() {
            Some(false)
        } else {
            self.client.panes().ok().map(|panes| {
                panes
                    .iter()
                    .all(|pane| Some(&pane.tab_id) != record.tab_id.as_ref())
            })
        };
        Ok(
            json!({"name":agent_name,"archive":destination,"pane_closed":!owned.is_empty(),"tab_closed":tab_closed}),
        )
    }
}

/// Seconds after the newest confirmed prompt delivery during which an idle agent counts as ready
/// only once `wait` has seen it busy. A delivery is confirmed when the prompt has left the composer,
/// which can precede the harness visibly starting its turn.
pub const DELIVERY_SETTLE_SECONDS: f64 = 10.0;

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Return the newest `confirmed_at` among processed queue entries, if any.
fn latest_confirmed_delivery(queue: &Path) -> Option<f64> {
    let entries = fs::read_dir(queue.join("processed")).ok()?;
    let mut latest: Option<f64> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(document) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(value) = document.get("confirmed_at").and_then(Value::as_f64) {
            if value.is_finite() {
                latest = Some(latest.map_or(value, |current| current.max(value)));
            }
        }
    }
    latest
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::{AdapterError, Result as AdapterResult};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    #[derive(Default)]
    struct CancelLifecycleWait {
        cancelled: AtomicBool,
    }

    impl agent::AgentRuntime for CancelLifecycleWait {
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }

        fn sleep(&self, _duration: Duration) {
            self.cancelled.store(true, Ordering::SeqCst);
        }

        fn cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }
    /// A registry over a fake Herdr client; the chat service's tests use it too.
    pub(crate) struct Fixture {
        pub(crate) root: PathBuf,
        pub(crate) client: Fake,
    }
    impl Fixture {
        pub(crate) fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "agentctl-managed-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                client: Fake {
                    root: root.clone(),
                    panes: Mutex::new(Vec::new()),
                    named_pane: Mutex::new("owned".to_owned()),
                    moves: Mutex::new(Vec::new()),
                    panes_calls: AtomicU64::new(0),
                    pane_info_calls: AtomicU64::new(0),
                    runs: Mutex::new(Vec::new()),
                    slot_commands: Mutex::new(Vec::new()),
                    environments: Mutex::new(Vec::new()),
                    closed: Mutex::new(Vec::new()),
                    focused: Mutex::new(Vec::new()),
                    fail_panes: AtomicBool::new(false),
                    fail_pane_info_once: Arc::new(AtomicBool::new(false)),
                    add_sibling_on_read: AtomicBool::new(false),
                    require_start_lock: AtomicBool::new(false),
                    started: AtomicBool::new(false),
                    fresh_presentations: AtomicBool::new(false),
                    dead_panes: Mutex::new(BTreeSet::new()),
                    stale_reported_panes: Mutex::new(BTreeSet::new()),
                    liveness_unknown: AtomicBool::new(false),
                    custom_processes: Mutex::new(BTreeMap::new()),
                    launches: Mutex::new(BTreeMap::new()),
                    session_override: Mutex::new(None),
                    report_session: AtomicBool::new(true),
                    duplicate_session: AtomicBool::new(false),
                    change_session_after_save: AtomicBool::new(false),
                    change_owned_session_after_save: AtomicBool::new(false),
                    fail_close: AtomicBool::new(false),
                    fail_after_close: AtomicBool::new(false),
                    custom_reported: AtomicBool::new(false),
                    custom_alive: AtomicBool::new(false),
                    custom_dies_after_report: AtomicBool::new(false),
                    custom_fails_after_identity: AtomicBool::new(false),
                    custom_at_idle_shell: AtomicBool::new(true),
                    foreign_shell_identity: Mutex::new(Fake::foreign_shell_identity()),
                    foreign_shell_path: Mutex::new(PathBuf::from("/bin/bash")),
                    replacement_shell_on_read: Mutex::new(None),
                    change_foreign_shell_after_save: AtomicBool::new(false),
                    change_foreign_shell_on_read: AtomicBool::new(false),
                    change_record_token_on_read: AtomicBool::new(false),
                    add_null_shell_identity_on_read: AtomicBool::new(false),
                    replace_record_directory_on_read: AtomicBool::new(false),
                    fail_read: AtomicBool::new(false),
                    fail_shell_proof: AtomicBool::new(false),
                    shell_proof_mutation: AtomicU64::new(0),
                    oversized_read: AtomicBool::new(false),
                    non_ascii_read: AtomicBool::new(false),
                    restart_foreign_on_read: AtomicBool::new(false),
                    leave_idle_shell_on_read: AtomicBool::new(false),
                    wrong_foreign_cwd: AtomicBool::new(false),
                    move_name_follows: AtomicBool::new(true),
                    move_return_matches: AtomicBool::new(true),
                    move_fails_before_change: AtomicBool::new(false),
                    fail_first_wait: AtomicBool::new(false),
                    scroll: Mutex::new(None),
                    read_sources: Mutex::new(Vec::new()),
                    screens: Mutex::new(std::collections::VecDeque::new()),
                    screen: Mutex::new(None),
                    unwrapped_empty: AtomicBool::new(false),
                    status: Mutex::new(None),
                    screen_after_run: Mutex::new(None),
                    labels: Mutex::new(BTreeMap::new()),
                    terminals: Mutex::new(BTreeMap::new()),
                    harness_pids: Mutex::new(BTreeMap::new()),
                    missing_harness_identity: AtomicBool::new(false),
                    missing_terminal: AtomicBool::new(false),
                    replace_harness_after_pin: AtomicBool::new(false),
                    replace_harness_after_save: AtomicBool::new(false),
                    replace_harness_on_read: AtomicBool::new(false),
                    literal_pastes: Mutex::new(Vec::new()),
                    muse_editor: Mutex::new(None),
                    herdr_names: Mutex::new(BTreeMap::new()),
                    expect_supported: AtomicBool::new(false),
                    scrollback: Mutex::new(BTreeMap::new()),
                    redirect_once: Mutex::new(BTreeMap::new()),
                    keys_sent: Mutex::new(Vec::new()),
                    replace_harness_after_esc: AtomicBool::new(false),
                    replace_harness_on_wait: AtomicBool::new(false),
                    drop_note: AtomicBool::new(false),
                    scrollback_on_effect: Mutex::new(None),
                    fail_scrollback: Mutex::new(None),
                    expected_terminals: Mutex::new(Vec::new()),
                    replace_harness_before_effect: AtomicBool::new(false),
                    swap_terminal_before_effect: AtomicBool::new(false),
                    fail_rename_tab: AtomicBool::new(false),
                    herdr_closed: Mutex::new(BTreeSet::new()),
                    print_after: Mutex::new(None),
                    printing: Mutex::new(Vec::new()),
                    printed_without: Mutex::new(Vec::new()),
                },
                root,
            }
        }
        pub(crate) fn manager(&self) -> ManagedAgents<'_, Fake> {
            ManagedAgents::new(&self.client, &self.root.join("registry"))
                .unwrap()
                .with_inherited_workspace(None)
        }
        pub(crate) fn start(&self, brief: Option<String>) -> Value {
            self.manager()
                .start(
                    "worker",
                    &self.root,
                    StartOptions {
                        workspace_id: Some("workspace".to_owned()),
                        brief,
                        ..StartOptions::default()
                    },
                )
                .unwrap()
        }
        fn prepare_foreign(&self) {
            self.client.panes.lock().unwrap().push(Fake::pane("owned"));
            self.client.started.store(true, Ordering::Relaxed);
        }
        fn adopt_options(&self) -> AdoptOptions {
            AdoptOptions {
                pane_id: "owned".to_owned(),
                expected_workspace: "subagents".to_owned(),
                cwd: self.root.clone(),
                harness: "codex".to_owned(),
                session: None,
            }
        }
        fn adopt(&self) -> Value {
            self.prepare_foreign();
            self.manager()
                .adopt("foreign", self.adopt_options())
                .unwrap()
        }

        fn make_legacy_dead(&self) -> (String, String, Vec<u8>) {
            let adopted = self.adopt();
            let path = self.root.join("registry/foreign/agent.json");
            let mut document = agent::read_private_json(&path).unwrap();
            document
                .as_object_mut()
                .unwrap()
                .remove("foreign_shell_identity");
            agent::atomic_json(&path, &document).unwrap();
            let raw = fs::read(&path).unwrap();
            self.client.started.store(false, Ordering::Relaxed);
            self.client.report_session.store(false, Ordering::Relaxed);
            (
                adopted["token"].as_str().unwrap().to_owned(),
                format!("{:x}", Sha256::digest(&raw)),
                raw,
            )
        }

        fn make_managed_dead(&self) -> (String, String) {
            let started = self.start(None);
            self.client.started.store(false, Ordering::Relaxed);
            self.client.report_session.store(false, Ordering::Relaxed);
            (
                started["pane_id"].as_str().unwrap().to_owned(),
                started["token"].as_str().unwrap().to_owned(),
            )
        }
    }

    #[test]
    fn service_cancellation_bounds_lifecycle_lock_contention() {
        let fixture = Fixture::new();
        let registry = fixture.root.join("registry");
        let manager = ManagedAgents::new(&fixture.client, &registry).expect("manager");
        agent::create_private_directory(&registry, "agent registry", true, true).expect("registry");
        let path = registry.join(".coordinator.lock");
        let holder = agent::open_private_lock(&path, "held lifecycle lock").expect("holder");
        holder.lock_exclusive().expect("hold lifecycle lock");
        let runtime = CancelLifecycleWait::default();
        let started = Instant::now();
        let error = manager
            .lock_with_runtime("coordinator", &runtime)
            .expect_err("cancel contended lifecycle lock");
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    pub(crate) struct Fake {
        root: PathBuf,
        panes: Mutex<Vec<Pane>>,
        named_pane: Mutex<String>,
        moves: Mutex<Vec<(String, String, String)>>,
        /// How many times `panes` was called.
        pub(crate) panes_calls: AtomicU64,
        /// How many times `pane_info` was called.
        pub(crate) pane_info_calls: AtomicU64,
        /// Every text written to a pane with `run`, such as a prompt, in order, and a line for
        /// each agent state reported to herdr.
        pub(crate) runs: Mutex<Vec<String>>,
        slot_commands: Mutex<Vec<(String, String)>>,
        environments: Mutex<Vec<Vec<String>>>,
        closed: Mutex<Vec<String>>,
        focused: Mutex<Vec<String>>,
        /// Whether every `panes` and `pane_info` call fails.
        pub(crate) fail_panes: AtomicBool,
        /// Whether the next `pane_info` call fails, which clears it. An `Arc`, so a stand-in for
        /// Herdr's server can set it from its own thread.
        pub(crate) fail_pane_info_once: Arc<AtomicBool>,
        add_sibling_on_read: AtomicBool,
        require_start_lock: AtomicBool,
        started: AtomicBool,
        fresh_presentations: AtomicBool,
        dead_panes: Mutex<BTreeSet<String>>,
        stale_reported_panes: Mutex<BTreeSet<String>>,
        liveness_unknown: AtomicBool,
        custom_processes: Mutex<BTreeMap<String, CustomProcessIdentity>>,
        launches: Mutex<BTreeMap<String, (String, Vec<String>)>>,
        session_override: Mutex<Option<(Option<String>, Option<String>)>>,
        report_session: AtomicBool,
        duplicate_session: AtomicBool,
        change_session_after_save: AtomicBool,
        change_owned_session_after_save: AtomicBool,
        fail_close: AtomicBool,
        fail_after_close: AtomicBool,
        custom_reported: AtomicBool,
        custom_alive: AtomicBool,
        custom_dies_after_report: AtomicBool,
        custom_fails_after_identity: AtomicBool,
        custom_at_idle_shell: AtomicBool,
        foreign_shell_identity: Mutex<CustomProcessIdentity>,
        foreign_shell_path: Mutex<PathBuf>,
        replacement_shell_on_read: Mutex<Option<PaneShellProof>>,
        change_foreign_shell_after_save: AtomicBool,
        change_foreign_shell_on_read: AtomicBool,
        change_record_token_on_read: AtomicBool,
        add_null_shell_identity_on_read: AtomicBool,
        replace_record_directory_on_read: AtomicBool,
        /// Whether every read fails.
        pub(crate) fail_read: AtomicBool,
        fail_shell_proof: AtomicBool,
        shell_proof_mutation: AtomicU64,
        oversized_read: AtomicBool,
        non_ascii_read: AtomicBool,
        restart_foreign_on_read: AtomicBool,
        leave_idle_shell_on_read: AtomicBool,
        wrong_foreign_cwd: AtomicBool,
        move_name_follows: AtomicBool,
        move_return_matches: AtomicBool,
        move_fails_before_change: AtomicBool,
        fail_first_wait: AtomicBool,
        pub(crate) scroll: Mutex<Option<crate::client::PaneScroll>>,
        pub(crate) read_sources: Mutex<Vec<String>>,
        /// The texts that successive reads return, first to last, before `screen` applies.
        pub(crate) screens: Mutex<std::collections::VecDeque<String>>,
        /// The text a read returns, when set; `visible output` otherwise.
        pub(crate) screen: Mutex<Option<String>>,
        /// Whether a `recent-unwrapped` read returns no text, as herdr's does for a pane it
        /// cannot serve that source for.
        pub(crate) unwrapped_empty: AtomicBool,
        /// The agent status `pane_info` reports, when set; `idle` otherwise.
        pub(crate) status: Mutex<Option<String>>,
        /// The text `screen` becomes once the next text is written with `run`, when set, as a
        /// typed prompt and the turn it starts push the rows above them up.
        pub(crate) screen_after_run: Mutex<Option<String>>,
        /// Tab labels by tab id.
        labels: Mutex<BTreeMap<String, String>>,
        /// Terminal ids by pane, overriding the default `term-PANE`.
        terminals: Mutex<BTreeMap<String, String>>,
        /// Foreground harness pid per pane; a test replaces the harness by changing it.
        harness_pids: Mutex<BTreeMap<String, u64>>,
        missing_harness_identity: AtomicBool,
        missing_terminal: AtomicBool,
        replace_harness_after_pin: AtomicBool,
        replace_harness_after_save: AtomicBool,
        replace_harness_on_read: AtomicBool,
        literal_pastes: Mutex<Vec<(String, String)>>,
        muse_editor: Mutex<Option<herdr_compatibility::MuseEditor>>,
        /// Herdr agent names mapped to their panes.
        herdr_names: Mutex<BTreeMap<String, String>>,
        /// Whether the server advertises `input-expect`.
        expect_supported: AtomicBool,
        /// Text each pane has shown in its scrollback.
        pub(crate) scrollback: Mutex<BTreeMap<String, Vec<String>>>,
        /// One-shot: a prompt addressed to the key pane is written to the value pane.
        pub(crate) redirect_once: Mutex<BTreeMap<String, String>>,
        /// Every key sent outside the guarded input path, with its pane.
        pub(crate) keys_sent: Mutex<Vec<(String, String)>>,
        /// Replace the pane's harness right after the next Esc reaches it.
        pub(crate) replace_harness_after_esc: AtomicBool,
        /// Replace the pane's harness as the next status wait reports its state.
        pub(crate) replace_harness_on_wait: AtomicBool,
        /// Whether a countermand note never shows in any pane.
        pub(crate) drop_note: AtomicBool,
        /// One-shot: replace a pane's scrollback as the next input effect reaches its target.
        pub(crate) scrollback_on_effect: Mutex<Option<(String, Vec<String>)>>,
        /// A pane whose scrollback cannot be read.
        pub(crate) fail_scrollback: Mutex<Option<String>>,
        /// The expected terminal sent with each input effect, in order.
        expected_terminals: Mutex<Vec<Option<String>>>,
        /// Replace the pane's harness as the next input effect reaches it.
        replace_harness_before_effect: AtomicBool,
        /// Give the pane another terminal as the next input effect reaches it.
        swap_terminal_before_effect: AtomicBool,
        /// Whether `rename_tab` fails, as a crash inside a rename would.
        fail_rename_tab: AtomicBool,
        /// Workspaces and panes Herdr has closed: lookups fail with its not-found codes.
        pub(crate) herdr_closed: Mutex<BTreeSet<String>>,
        /// How long after its submission a prompt shows in the pane's scrollback, as a harness
        /// that is still starting prints it; `None` shows it at once, `Duration::MAX` never.
        pub(crate) print_after: Mutex<Option<Duration>>,
        /// Prompts submitted but not shown yet: pane, printed text, and when it shows.
        printing: Mutex<Vec<(String, String, Option<std::time::Instant>)>>,
        /// Characters a harness drops when it prints a prompt, as Markdown rendering does.
        pub(crate) printed_without: Mutex<Vec<char>>,
    }
    impl Fake {
        fn terminal(&self, pane: &str) -> String {
            self.terminals
                .lock()
                .unwrap()
                .get(pane)
                .cloned()
                .unwrap_or_else(|| format!("term-{pane}"))
        }

        fn harness(pid: u64) -> CustomProcessIdentity {
            CustomProcessIdentity {
                version: 1,
                boot_id: "22222222-3333-4444-5555-666666666666".to_owned(),
                pid,
                starttime_ticks: pid,
                executable_device: 5,
                executable_inode: 6,
            }
        }

        /// One input effect reaching `pane`, refused like Herdr when the terminal differs.
        fn effect(&self, pane: &str, expect_terminal: Option<&str>) -> AdapterResult<()> {
            if self
                .replace_harness_before_effect
                .swap(false, Ordering::SeqCst)
            {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), 999);
            }
            if self
                .swap_terminal_before_effect
                .swap(false, Ordering::SeqCst)
            {
                self.terminals
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), "term-other".to_owned());
            }
            self.expected_terminals
                .lock()
                .unwrap()
                .push(expect_terminal.map(str::to_owned));
            if expect_terminal.is_some_and(|terminal| terminal != self.terminal(pane)) {
                return Err(AdapterError::expectation_failed(format!(
                    r#"agent prompt {pane}: {{"error":{{"code":"expectation_failed"}}}}"#
                )));
            }
            Ok(())
        }

        fn pane(id: &str) -> Pane {
            Pane {
                pane_id: id.to_owned(),
                tab_id: "tab".to_owned(),
                workspace_id: "workspace".to_owned(),
            }
        }

        fn custom_identity() -> CustomProcessIdentity {
            CustomProcessIdentity {
                version: 1,
                boot_id: "11111111-2222-3333-4444-555555555555".to_owned(),
                pid: 4242,
                starttime_ticks: 9001,
                executable_device: 7,
                executable_inode: 11,
            }
        }

        fn foreign_shell_identity() -> CustomProcessIdentity {
            CustomProcessIdentity {
                version: 1,
                boot_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_owned(),
                pid: 100,
                starttime_ticks: 8001,
                executable_device: 13,
                executable_inode: 17,
            }
        }
    }
    impl AgentApi for Fake {
        fn codex_idle_ready_with_runtime(
            &self,
            info: &AgentPaneInfo,
            runtime: &dyn agent::AgentRuntime,
        ) -> AdapterResult<bool> {
            if runtime.cancelled()
                || info.agent.as_deref() != Some("codex")
                || info.status != "unknown"
            {
                return Ok(false);
            }
            let Some(identity) = self.harness_identity(&info.pane_id, "codex")? else {
                return Ok(false);
            };
            let screen = self.read(&info.pane_id, "visible", Some(200))?;
            Ok(crate::submission::codex_idle_composer(&screen)
                && self.pane_info(&info.pane_id)? == *info
                && self.verify_harness_identity(&info.pane_id, &identity)?)
        }

        fn panes(&self) -> AdapterResult<Vec<Pane>> {
            self.panes_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_panes.load(Ordering::Relaxed) {
                return Err(AdapterError::unavailable(
                    "pane query failed after allocation",
                ));
            }
            if !self.harness_pids.lock().unwrap().is_empty()
                && self
                    .replace_harness_after_pin
                    .swap(false, Ordering::Relaxed)
            {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert("owned".to_owned(), 999);
            }
            Ok(self.panes.lock().unwrap().clone())
        }
        fn pane_info(&self, pane: &str) -> AdapterResult<AgentPaneInfo> {
            self.pane_info_calls.fetch_add(1, Ordering::Relaxed);
            if self.herdr_closed.lock().unwrap().contains(pane) {
                return Err(AdapterError::unavailable(format!(
                    r#"pane get: {{"error": {{"code": "pane_not_found", "message": "pane {pane} not found"}}}}"#
                )));
            }
            let fail_once = self.fail_pane_info_once.swap(false, Ordering::SeqCst);
            if self.fail_panes.load(Ordering::Relaxed) || fail_once {
                return Err(AdapterError::unavailable(
                    "pane query failed after allocation",
                ));
            }
            if self.require_start_lock.load(Ordering::Relaxed) {
                let lock = agent::open_private_lock(
                    &self.root.join("registry/.worker.lock"),
                    "test lifecycle lock",
                )
                .unwrap();
                assert!(
                    FileExt::try_lock_exclusive(&lock).is_err(),
                    "startup released its generation lock before completion"
                );
            }
            if !self.panes.lock().unwrap().iter().any(|p| p.pane_id == pane) {
                return Err(AdapterError::unavailable("missing pane"));
            }
            let changed_after_save = self.change_session_after_save.load(Ordering::Relaxed)
                && self.root.join("registry/foreign/agent.json").exists();
            let owned_changed_after_save =
                self.change_owned_session_after_save.load(Ordering::Relaxed)
                    && agent::read_private_json(&self.root.join("registry/worker/agent.json"))
                        .ok()
                        .and_then(|record| record["lifecycle"].as_str().map(str::to_owned))
                        .as_deref()
                        == Some("running");
            let changed_after_save = changed_after_save || owned_changed_after_save;
            let dead = self.dead_panes.lock().unwrap().contains(pane);
            let stale_report = self.stale_reported_panes.lock().unwrap().contains(pane);
            let report_session = stale_report
                || (!dead
                    && (self.report_session.load(Ordering::Relaxed)
                        || changed_after_save
                        || pane == "reported"));
            let launch = self.launches.lock().unwrap().get(pane).cloned();
            let kind = launch
                .as_ref()
                .map(|(kind, _)| kind.as_str())
                .unwrap_or_else(|| {
                    if self.custom_reported.load(Ordering::Relaxed) {
                        "muse"
                    } else if pane == "claude" {
                        "claude"
                    } else {
                        "codex"
                    }
                });
            let session_override = self.session_override.lock().unwrap().clone();
            let requested_session = launch.as_ref().and_then(|(_, arguments)| {
                arguments.windows(2).find_map(|pair| {
                    matches!(pair[0].as_str(), "--session-id" | "--resume" | "resume")
                        .then(|| pair[1].clone())
                })
            });
            let reported_agent = if report_session {
                session_override
                    .as_ref()
                    .map_or_else(|| Some(kind.to_owned()), |(agent, _)| agent.clone())
            } else {
                None
            };
            let reported_value = if !report_session {
                None
            } else if changed_after_save {
                Some("replacement-thread".to_owned())
            } else if let Some((_, value)) = session_override {
                value
            } else if let Some(value) = requested_session {
                Some(value)
            } else if matches!(pane, "owned" | "claude" | "reported" | "project-pane")
                || self.duplicate_session.load(Ordering::Relaxed)
            {
                Some("thread".to_owned())
            } else {
                Some("human-thread".to_owned())
            };
            let workspace_id = self
                .panes
                .lock()
                .unwrap()
                .iter()
                .find(|entry| entry.pane_id == pane)
                .map(|entry| entry.workspace_id.clone())
                .unwrap_or_else(|| {
                    if pane == "external" {
                        "other-workspace".to_owned()
                    } else {
                        "workspace".to_owned()
                    }
                });
            Ok(AgentPaneInfo {
                pane_id: pane.to_owned(),
                workspace_id,
                cwd: if self.wrong_foreign_cwd.load(Ordering::Relaxed) {
                    self.root.join("other").display().to_string()
                } else {
                    self.root.display().to_string()
                },
                agent: (stale_report || (self.started.load(Ordering::Relaxed) && !dead))
                    .then(|| kind.to_owned()),
                status: self
                    .status
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "idle".to_owned()),
                session_agent: reported_agent,
                session_value: reported_value,
                scroll: *self.scroll.lock().unwrap(),
                terminal_id: (!self.missing_terminal.load(Ordering::Relaxed))
                    .then(|| self.terminal(pane)),
                tab_id: self
                    .panes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|entry| entry.pane_id == pane)
                    .map(|entry| entry.tab_id.clone()),
            })
        }
        fn workspace_label(&self, workspace: &str) -> AdapterResult<String> {
            Ok(if workspace == "project-workspace" {
                "project-agents"
            } else {
                "subagents"
            }
            .to_owned())
        }
        fn run(&self, _: &str, text: &str) -> AdapterResult<()> {
            self.runs.lock().unwrap().push(text.to_owned());
            if let Some(screen) = self.screen_after_run.lock().unwrap().take() {
                *self.screen.lock().unwrap() = Some(screen);
            }
            Ok(())
        }
        fn wait_agent_status(&self, pane: &str, _: &str, _: u64) -> AdapterResult<()> {
            if self.replace_harness_on_wait.swap(false, Ordering::SeqCst) {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), 999);
            }
            if self.fail_first_wait.swap(false, Ordering::Relaxed) {
                return Err(AdapterError::unavailable("injected first wait failure"));
            }
            Ok(())
        }
        fn read(&self, pane: &str, source: &str, _: Option<usize>) -> AdapterResult<String> {
            self.read_sources.lock().unwrap().push(source.to_owned());
            if self.fail_read.load(Ordering::Relaxed) {
                return Err(AdapterError::unavailable("capture failed"));
            }
            if source == "recent-unwrapped" && self.unwrapped_empty.load(Ordering::Relaxed) {
                return Ok(String::new());
            }
            if self.replace_harness_on_read.swap(false, Ordering::Relaxed) {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), 999);
            }
            if self.oversized_read.load(Ordering::Relaxed) {
                return Ok("\0".repeat(3 << 20));
            }
            if self.non_ascii_read.load(Ordering::Relaxed) {
                return Ok("é".repeat(3 << 20));
            }
            if self.add_sibling_on_read.swap(false, Ordering::Relaxed) {
                self.panes.lock().unwrap().push(Self::pane("human"));
            }
            if self.restart_foreign_on_read.swap(false, Ordering::Relaxed) {
                self.started.store(true, Ordering::Relaxed);
                self.report_session.store(true, Ordering::Relaxed);
            }
            if self.leave_idle_shell_on_read.swap(false, Ordering::Relaxed) {
                self.custom_at_idle_shell.store(false, Ordering::Relaxed);
            }
            if self
                .change_foreign_shell_on_read
                .swap(false, Ordering::Relaxed)
            {
                self.foreign_shell_identity.lock().unwrap().starttime_ticks += 1;
            }
            if let Some(replacement) = self.replacement_shell_on_read.lock().unwrap().take() {
                *self.foreign_shell_identity.lock().unwrap() = replacement.identity;
                *self.foreign_shell_path.lock().unwrap() = replacement.executable_path;
            }
            if self
                .change_record_token_on_read
                .swap(false, Ordering::Relaxed)
            {
                for name in ["foreign", "worker"] {
                    let path = self.root.join(format!("registry/{name}/agent.json"));
                    if path.is_file() {
                        let mut document = agent::read_private_json(&path).unwrap();
                        document["token"] = json!("replacement-generation");
                        agent::atomic_json(&path, &document).unwrap();
                    }
                }
            }
            if self
                .add_null_shell_identity_on_read
                .swap(false, Ordering::Relaxed)
            {
                let path = self.root.join("registry/foreign/agent.json");
                if path.is_file() {
                    let mut document = agent::read_private_json(&path).unwrap();
                    document["foreign_shell_identity"] = Value::Null;
                    agent::atomic_json(&path, &document).unwrap();
                }
            }
            if self
                .replace_record_directory_on_read
                .swap(false, Ordering::Relaxed)
            {
                for name in ["foreign", "worker"] {
                    let active = self.root.join(format!("registry/{name}"));
                    if active.is_dir() {
                        let displaced = self.root.join(format!("registry/.{name}-displaced"));
                        fs::rename(&active, &displaced).unwrap();
                        fs::create_dir(&active).unwrap();
                        fs::set_permissions(&active, fs::Permissions::from_mode(0o700)).unwrap();
                        fs::copy(displaced.join("agent.json"), active.join("agent.json")).unwrap();
                        fs::set_permissions(
                            active.join("agent.json"),
                            fs::Permissions::from_mode(0o600),
                        )
                        .unwrap();
                    }
                }
            }
            if let Some(text) = self.screens.lock().unwrap().pop_front() {
                return Ok(text);
            }
            if let Some(editor) = self.muse_editor.lock().unwrap().as_ref() {
                return Ok(editor.screen());
            }
            Ok(self
                .screen
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "visible output".to_owned()))
        }
    }
    impl ManagedApi for Fake {
        fn process_liveness(&self, expected: &CustomProcessIdentity) -> ProcessLiveness {
            if self.liveness_unknown.load(Ordering::Relaxed) {
                return ProcessLiveness::Unknown;
            }
            let dead = self.dead_panes.lock().unwrap();
            for (pane, pid) in self.harness_pids.lock().unwrap().iter() {
                if Self::harness(*pid) == *expected {
                    return if dead.contains(pane) {
                        ProcessLiveness::Dead
                    } else if self.started.load(Ordering::Relaxed) {
                        ProcessLiveness::Alive
                    } else {
                        ProcessLiveness::Unknown
                    };
                }
            }
            for (pane, identity) in self.custom_processes.lock().unwrap().iter() {
                if identity == expected {
                    return if dead.contains(pane) {
                        ProcessLiveness::Dead
                    } else {
                        ProcessLiveness::Alive
                    };
                }
            }
            ProcessLiveness::Unknown
        }
        fn workspace_id_for_label(&self, label: &str) -> AdapterResult<Option<String>> {
            Ok(match label {
                "subagents" => Some("workspace".to_owned()),
                "project-agents" => Some("project-workspace".to_owned()),
                _ => None,
            })
        }
        fn create_workspace(
            &self,
            _: &str,
            _: &str,
            _: &[String],
        ) -> AdapterResult<(String, String, String)> {
            unreachable!()
        }
        fn create_tab(
            &self,
            workspace: &str,
            label: &str,
            cwd: &str,
            environment: &[String],
        ) -> AdapterResult<String> {
            self.create_tab_with_pane(workspace, label, cwd, environment)
                .map(|value| value.0)
        }
        fn create_tab_with_pane(
            &self,
            workspace: &str,
            label: &str,
            _: &str,
            environment: &[String],
        ) -> AdapterResult<(String, String)> {
            let allocation = {
                let mut environments = self.environments.lock().unwrap();
                environments.push(environment.to_vec());
                environments.len()
            };
            let fresh = self.fresh_presentations.load(Ordering::Relaxed) && allocation > 1;
            let tab = if fresh {
                format!("tab-{allocation}")
            } else {
                "tab".to_owned()
            };
            let pane = if fresh {
                format!("owned-{allocation}")
            } else {
                "owned".to_owned()
            };
            self.labels
                .lock()
                .unwrap()
                .insert(tab.clone(), label.to_owned());
            self.panes.lock().unwrap().push(Pane {
                pane_id: pane.clone(),
                tab_id: tab.clone(),
                workspace_id: workspace.to_owned(),
            });
            Ok((tab, pane))
        }
        fn move_pane_to_new_tab(
            &self,
            pane: &str,
            workspace: &str,
            label: &str,
        ) -> AdapterResult<Pane> {
            if self.move_fails_before_change.load(Ordering::Relaxed) {
                return Err(AdapterError::unavailable("move refused before mutation"));
            }
            let mut panes = self.panes.lock().unwrap();
            let source = panes
                .iter()
                .position(|entry| entry.pane_id == pane)
                .ok_or_else(|| AdapterError::unavailable("missing source pane"))?;
            panes.remove(source);
            let moved = Pane {
                pane_id: "project-pane".to_owned(),
                tab_id: "project-tab".to_owned(),
                workspace_id: workspace.to_owned(),
            };
            panes.push(moved.clone());
            self.labels
                .lock()
                .unwrap()
                .insert(moved.tab_id.clone(), label.to_owned());
            let terminal = self.terminal(pane);
            self.terminals
                .lock()
                .unwrap()
                .insert(moved.pane_id.clone(), terminal);
            let pid = self.harness_pids.lock().unwrap().remove(pane);
            if let Some(pid) = pid {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert(moved.pane_id.clone(), pid);
            }
            if self.move_name_follows.load(Ordering::Relaxed) {
                *self.named_pane.lock().unwrap() = moved.pane_id.clone();
                for owner in self.herdr_names.lock().unwrap().values_mut() {
                    if owner == pane {
                        owner.clone_from(&moved.pane_id);
                    }
                }
            }
            self.moves.lock().unwrap().push((
                pane.to_owned(),
                workspace.to_owned(),
                label.to_owned(),
            ));
            if self.move_return_matches.load(Ordering::Relaxed) {
                Ok(moved)
            } else {
                Ok(Pane {
                    tab_id: "reported-wrong-tab".to_owned(),
                    ..moved
                })
            }
        }
        fn close_pane(&self, pane: &str) -> AdapterResult<()> {
            if self.fail_close.load(Ordering::Relaxed) {
                return Err(AdapterError::unavailable("close failed"));
            }
            self.closed.lock().unwrap().push(pane.to_owned());
            self.panes.lock().unwrap().retain(|p| p.pane_id != pane);
            if self.fresh_presentations.load(Ordering::Relaxed) {
                self.herdr_closed.lock().unwrap().insert(pane.to_owned());
            }
            if self.fail_after_close.swap(false, Ordering::Relaxed) {
                return Err(AdapterError::unavailable(
                    "injected result loss after close",
                ));
            }
            Ok(())
        }
        fn focus_pane(&self, pane: &str) -> AdapterResult<()> {
            self.focused.lock().unwrap().push(pane.to_owned());
            Ok(())
        }
        fn rename_tab(&self, tab: &str, label: &str) -> AdapterResult<()> {
            if self.fail_rename_tab.load(Ordering::SeqCst) {
                return Err(AdapterError::unavailable("simulated crash inside rename"));
            }
            self.labels
                .lock()
                .unwrap()
                .insert(tab.to_owned(), label.to_owned());
            Ok(())
        }
        fn agent_identity(&self, name: &str) -> AdapterResult<AgentIdentity> {
            let pane = self.agent_pane(name)?;
            let tab = self
                .panes
                .lock()
                .unwrap()
                .iter()
                .find(|entry| entry.pane_id == pane)
                .map(|entry| entry.tab_id.clone());
            Ok(AgentIdentity {
                name: name.to_owned(),
                terminal_id: Some(self.terminal(&pane)),
                pane_id: pane,
                tab_id: tab,
            })
        }
        fn rename_agent(&self, pane: &str, name: &str) -> AdapterResult<()> {
            let mut names = self.herdr_names.lock().unwrap();
            names.retain(|_, owner| owner != pane);
            names.insert(name.to_owned(), pane.to_owned());
            Ok(())
        }
        fn agent_names(&self) -> AdapterResult<BTreeMap<String, String>> {
            let live: BTreeSet<String> = self
                .panes
                .lock()
                .unwrap()
                .iter()
                .map(|pane| pane.pane_id.clone())
                .collect();
            Ok(self
                .herdr_names
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, pane)| live.contains(*pane))
                .map(|(name, pane)| (name.clone(), pane.clone()))
                .collect())
        }
        fn tab_label(&self, tab: &str) -> AdapterResult<String> {
            self.labels
                .lock()
                .unwrap()
                .get(tab)
                .cloned()
                .ok_or_else(|| AdapterError::unavailable(format!("missing tab {tab}")))
        }
        fn tab_labels(&self, workspace: &str) -> AdapterResult<BTreeMap<String, String>> {
            if self.herdr_closed.lock().unwrap().contains(workspace) {
                return Err(AdapterError::unavailable(format!(
                    r#"tab list: {{"error":{{"code":"workspace_not_found","message":"workspace {workspace} not found"}}}}"#
                )));
            }
            let labels = self.labels.lock().unwrap();
            Ok(self
                .panes
                .lock()
                .unwrap()
                .iter()
                .filter(|pane| pane.workspace_id == workspace)
                .map(|pane| {
                    (
                        pane.tab_id.clone(),
                        labels.get(&pane.tab_id).cloned().unwrap_or_default(),
                    )
                })
                .collect())
        }
        fn harness_identity(
            &self,
            pane: &str,
            _kind: &str,
        ) -> AdapterResult<Option<CustomProcessIdentity>> {
            if self.missing_harness_identity.load(Ordering::Relaxed)
                || !self.started.load(Ordering::Relaxed)
                || self.dead_panes.lock().unwrap().contains(pane)
            {
                return Ok(None);
            }
            let mut pids = self.harness_pids.lock().unwrap();
            let next = 300 + pids.len() as u64;
            Ok(Some(Self::harness(
                *pids.entry(pane.to_owned()).or_insert(next),
            )))
        }
        fn verify_harness_identity(
            &self,
            pane: &str,
            expected: &CustomProcessIdentity,
        ) -> AdapterResult<bool> {
            if self.root.join("registry/foreign/agent.json").exists()
                && self
                    .replace_harness_after_save
                    .swap(false, Ordering::Relaxed)
            {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), 999);
            }
            Ok(self.started.load(Ordering::Relaxed)
                && !self.dead_panes.lock().unwrap().contains(pane)
                && self
                    .panes
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|entry| entry.pane_id == pane)
                && self.harness_pids.lock().unwrap().get(pane) == Some(&expected.pid))
        }
        fn input_expect_supported(&self) -> AdapterResult<bool> {
            Ok(self.expect_supported.load(Ordering::SeqCst))
        }
        fn agent_prompt_expect(
            &self,
            pane: &str,
            text: &str,
            expect_terminal: Option<&str>,
            _: &dyn agent::AgentRuntime,
        ) -> AdapterResult<()> {
            self.effect(pane, expect_terminal)?;
            if let Some((other, lines)) = self.scrollback_on_effect.lock().unwrap().take() {
                self.scrollback.lock().unwrap().insert(other, lines);
            }
            if text == MISROUTE_NOTE && self.drop_note.load(Ordering::SeqCst) {
                return self.run(pane, text);
            }
            let written = self
                .redirect_once
                .lock()
                .unwrap()
                .remove(pane)
                .unwrap_or_else(|| pane.to_owned());
            let dropped = self.printed_without.lock().unwrap().clone();
            let printed: String = text
                .chars()
                .filter(|char| !dropped.contains(char))
                .collect();
            match *self.print_after.lock().unwrap() {
                None => self
                    .scrollback
                    .lock()
                    .unwrap()
                    .entry(written)
                    .or_default()
                    .push(printed),
                Some(delay) => self.printing.lock().unwrap().push((
                    written,
                    printed,
                    std::time::Instant::now().checked_add(delay),
                )),
            }
            self.run(pane, text)
        }
        fn read_scrollback(&self, pane: &str) -> AdapterResult<String> {
            {
                let now = std::time::Instant::now();
                let mut scrollback = self.scrollback.lock().unwrap();
                self.printing
                    .lock()
                    .unwrap()
                    .retain(|(written, printed, at)| {
                        let due = at.is_some_and(|at| at <= now);
                        if due {
                            scrollback
                                .entry(written.clone())
                                .or_default()
                                .push(printed.clone());
                        }
                        !due
                    });
            }
            if self.herdr_closed.lock().unwrap().contains(pane) {
                return Err(AdapterError::unavailable(format!(
                    r#"pane read {pane}: {{"error":{{"code":"pane_not_found","message":"pane {pane} not found"}}}}"#
                )));
            }
            if self.fail_scrollback.lock().unwrap().as_deref() == Some(pane) {
                return Err(AdapterError::unavailable("scrollback read failed"));
            }
            Ok(self
                .scrollback
                .lock()
                .unwrap()
                .get(pane)
                .map(|lines| lines.join("\n"))
                .unwrap_or_default())
        }
        fn send_keys_expect(
            &self,
            pane: &str,
            key: &str,
            expect_terminal: Option<&str>,
            _: &dyn agent::AgentRuntime,
        ) -> AdapterResult<()> {
            self.effect(pane, expect_terminal)?;
            self.send_keys(pane, key)
        }
        fn send_text_expect(
            &self,
            pane: &str,
            text: &str,
            expect_terminal: Option<&str>,
            _: &dyn agent::AgentRuntime,
        ) -> AdapterResult<()> {
            self.effect(pane, expect_terminal)?;
            self.send_text(pane, text)
        }
        fn harness_executable(&self, kind: &str) -> AdapterResult<PathBuf> {
            Ok(PathBuf::from(format!("/opt/bin/{kind}")))
        }
        fn start_relay_agent(
            &self,
            kind: &str,
            pane: &str,
            command_line: &str,
            _: Duration,
            persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> AdapterResult<()>,
        ) -> AdapterResult<CustomProcessIdentity> {
            self.slot_commands
                .lock()
                .unwrap()
                .push((format!("relay:{kind}:{pane}"), command_line.to_owned()));
            self.started.store(true, Ordering::Relaxed);
            persist_identity(relay_identity())?;
            Ok(relay_identity())
        }
        fn relay_process(&self, _: &str) -> AdapterResult<Option<CustomProcessIdentity>> {
            Ok(self.started.load(Ordering::Relaxed).then(relay_identity))
        }
        fn explain_agent(&self, _: &str) -> AdapterResult<(Option<String>, String)> {
            Ok((Some("codex".to_owned()), "idle".to_owned()))
        }
        fn report_pane_agent(&self, pane: &str, kind: &str, state: &str) -> AdapterResult<()> {
            self.runs
                .lock()
                .unwrap()
                .push(format!("report {pane} {kind} {state}"));
            Ok(())
        }
        fn enter_slot_sandbox(
            &self,
            pane: &str,
            command_line: &str,
            _: Duration,
        ) -> AdapterResult<u64> {
            self.slot_commands
                .lock()
                .unwrap()
                .push((pane.to_owned(), command_line.to_owned()));
            Ok(4242)
        }
        fn start_agent(
            &self,
            name: &str,
            kind: &str,
            pane: &str,
            arguments: &[String],
            _: Duration,
        ) -> AdapterResult<()> {
            self.launches
                .lock()
                .unwrap()
                .insert(pane.to_owned(), (kind.to_owned(), arguments.to_vec()));
            self.herdr_names
                .lock()
                .unwrap()
                .insert(name.to_owned(), pane.to_owned());
            if self.require_start_lock.load(Ordering::Relaxed) {
                let identity = agent::open_private_lock(
                    &self.root.join("registry/.identity.lock"),
                    "test identity lock",
                )
                .unwrap();
                assert!(
                    FileExt::try_lock_exclusive(&identity).is_err(),
                    "startup released its identity lock before native session commit"
                );
            }
            self.started.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn start_pane_agent(
            &self,
            _: &str,
            kind: &str,
            pane: &str,
            arguments: &[String],
            _: Duration,
            persist_identity: &mut dyn FnMut(CustomProcessIdentity) -> AdapterResult<()>,
        ) -> AdapterResult<()> {
            self.launches
                .lock()
                .unwrap()
                .insert(pane.to_owned(), (kind.to_owned(), arguments.to_vec()));
            self.started.store(true, Ordering::Relaxed);
            self.custom_alive.store(true, Ordering::Relaxed);
            let mut identity = Self::custom_identity();
            if self.fresh_presentations.load(Ordering::Relaxed) {
                let generation = self.environments.lock().unwrap().len() as u64 - 1;
                identity.pid += generation;
                identity.starttime_ticks += generation;
                self.custom_processes
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), identity.clone());
            }
            persist_identity(identity)?;
            if self.custom_fails_after_identity.load(Ordering::Relaxed) {
                let record =
                    agent::read_private_json(&self.root.join("registry/worker/agent.json"))
                        .expect("identity callback persisted the starting record");
                assert_eq!(record["lifecycle"], "starting");
                assert_eq!(record["custom_process_identity"]["pid"], 4242);
                return Err(AdapterError::unavailable(
                    "Muse workspace trust prompt requires human attention; no input was submitted",
                ));
            }
            self.custom_reported.store(true, Ordering::Relaxed);
            self.custom_alive.store(
                !self.custom_dies_after_report.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            Ok(())
        }
        fn verify_custom_harness(
            &self,
            pane: &str,
            _: &str,
            identity: Option<&CustomProcessIdentity>,
        ) -> AdapterResult<()> {
            let custom = self
                .custom_processes
                .lock()
                .unwrap()
                .get(pane)
                .cloned()
                .unwrap_or_else(Self::custom_identity);
            if self.custom_alive.load(Ordering::Relaxed)
                && !self.dead_panes.lock().unwrap().contains(pane)
                && identity.is_none_or(|identity| identity == &custom)
            {
                Ok(())
            } else {
                Err(AdapterError::unavailable(
                    "custom harness is not the foreground process",
                ))
            }
        }
        fn pane_is_idle_shell(&self, _: &str) -> AdapterResult<bool> {
            Ok(!self.custom_alive.load(Ordering::Relaxed)
                && self.custom_at_idle_shell.load(Ordering::Relaxed))
        }
        fn pane_shell_identity(&self, _: &str) -> AdapterResult<CustomProcessIdentity> {
            let mut identity = self.foreign_shell_identity.lock().unwrap().clone();
            if self.change_foreign_shell_after_save.load(Ordering::Relaxed)
                && self.root.join("registry/foreign/agent.json").exists()
            {
                identity.starttime_ticks += 1;
            }
            Ok(identity)
        }
        fn pane_is_same_idle_shell(
            &self,
            _: &str,
            expected: &CustomProcessIdentity,
        ) -> AdapterResult<bool> {
            Ok(!self.custom_alive.load(Ordering::Relaxed)
                && self.custom_at_idle_shell.load(Ordering::Relaxed)
                && *expected == *self.foreign_shell_identity.lock().unwrap())
        }
        fn pane_idle_shell_identity(&self, pane: &str) -> AdapterResult<Option<PaneShellProof>> {
            if self.fail_shell_proof.load(Ordering::Relaxed) {
                return Err(AdapterError::unavailable("injected procfs proof failure"));
            }
            let proof = (self.dead_panes.lock().unwrap().contains(pane)
                || (!self.custom_alive.load(Ordering::Relaxed)
                    && self.custom_at_idle_shell.load(Ordering::Relaxed))
                    && !self.fresh_presentations.load(Ordering::Relaxed))
            .then(|| PaneShellProof {
                identity: self.foreign_shell_identity.lock().unwrap().clone(),
                executable_path: self.foreign_shell_path.lock().unwrap().clone(),
            });
            if self.root.join("registry/worker/output.json").exists() {
                match self.shell_proof_mutation.swap(0, Ordering::Relaxed) {
                    1 => self.panes.lock().unwrap().push(Self::pane("human")),
                    2 => self.started.store(true, Ordering::Relaxed),
                    _ => {}
                }
            }
            Ok(proof)
        }
        fn agent_pane(&self, name: &str) -> AdapterResult<String> {
            if self.fresh_presentations.load(Ordering::Relaxed) {
                if let Some(pane) = self.herdr_names.lock().unwrap().get(name) {
                    return Ok(pane.clone());
                }
            }
            Ok(self.named_pane.lock().unwrap().clone())
        }
        fn report_agent_session(&self, _: &str, _: &str, _: &str, _: &str) -> AdapterResult<()> {
            Ok(())
        }
        fn send_keys(&self, pane: &str, key: &str) -> AdapterResult<()> {
            self.keys_sent
                .lock()
                .unwrap()
                .push((pane.to_owned(), key.to_owned()));
            if key == "esc" && self.replace_harness_after_esc.swap(false, Ordering::SeqCst) {
                self.harness_pids
                    .lock()
                    .unwrap()
                    .insert(pane.to_owned(), 777);
            }
            if key == "Enter" {
                if let Some(prompt) = self
                    .muse_editor
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|editor| editor.enter())
                {
                    self.scrollback
                        .lock()
                        .unwrap()
                        .entry(pane.to_owned())
                        .or_default()
                        .push(prompt);
                }
            }
            Ok(())
        }
        fn send_text(&self, pane: &str, text: &str) -> AdapterResult<()> {
            let mut editor = self.muse_editor.lock().unwrap();
            let editor = editor
                .as_mut()
                .ok_or_else(|| AdapterError::unavailable("literal pane input is unavailable"))?;
            let prompt = text
                .strip_prefix(BRACKETED_PASTE_START)
                .and_then(|value| value.strip_suffix(BRACKETED_PASTE_END))
                .ok_or_else(|| AdapterError::unavailable("expected literal bracketed paste"))?;
            self.literal_pastes
                .lock()
                .unwrap()
                .push((pane.to_owned(), text.to_owned()));
            editor.paste(prompt);
            Ok(())
        }
        fn close_tab(&self, _: &str) -> AdapterResult<()> {
            panic!("managed lifecycle must close exactly the owned pane")
        }
    }

    #[test]
    fn startup_keeps_its_generation_lock_through_brief_and_returned_status() {
        let fixture = Fixture::new();
        fixture
            .client
            .require_start_lock
            .store(true, Ordering::Relaxed);
        let status = fixture.start(Some("initial task".to_owned()));
        assert_eq!(status["lifecycle"], "running");
        assert_eq!(*fixture.client.runs.lock().unwrap(), ["initial task"]);
    }

    #[test]
    fn environment_entries_preserve_literals_and_reject_invalid_input() {
        let entries = vec![
            "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai".to_owned(),
            "LITERAL= spaces $(unexpanded) = remain ".to_owned(),
            "UNICODE=snowman-☃\nnext-line".to_owned(),
            "EMPTY=".to_owned(),
        ];
        assert_eq!(environment_entries(&entries).unwrap(), entries);
        for invalid in [
            "MISSING_EQUALS",
            "=value",
            "9START=value",
            "BAD-NAME=value",
            "BAD\0NAME=value",
            "GOOD=bad\0value",
        ] {
            assert!(
                environment_entries(&[invalid.to_owned()]).is_err(),
                "accepted invalid environment entry {invalid:?}"
            );
        }
    }

    #[test]
    fn start_sets_environment_only_on_the_created_tab() {
        let fixture = Fixture::new();
        let entries = vec![
            "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai".to_owned(),
            "LITERAL=a b=$(unexpanded)=tail".to_owned(),
        ];
        let status = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    environment: entries.clone(),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        let observed = fixture.client.environments.lock().unwrap();
        assert_eq!(observed.as_slice(), std::slice::from_ref(&entries));
        drop(observed);
        assert!(status.get("environment").is_none());
        for entry in entries {
            assert!(!status.to_string().contains(&entry));
        }
    }

    #[test]
    fn invalid_environment_is_refused_before_registry_or_tab_creation() {
        let fixture = Fixture::new();
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    environment: vec!["BAD-NAME=value".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("environment variable name"));
        assert!(!fixture.root.join("registry").exists());
        assert!(fixture.client.environments.lock().unwrap().is_empty());
    }

    fn relay_identity() -> CustomProcessIdentity {
        CustomProcessIdentity {
            version: 1,
            boot_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            pid: 4242,
            starttime_ticks: 7,
            executable_device: 1,
            executable_inode: 2,
        }
    }

    #[test]
    fn root_slot_starts_the_harness_behind_the_relay() {
        let fixture = Fixture::new();
        // The fake Herdr reports every pane at the fixture root, so the slot is there.
        let executable = fake_wrkslots(&fixture.root, &fixture.root, "root");
        let status = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    slot: Some(SlotLaunch {
                        slot: "s1".to_owned(),
                        executable: Some(executable),
                        ..SlotLaunch::default()
                    }),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert_eq!(status["adapter"], "herdr-relay");
        assert_eq!(status["lifecycle"], "running");
        assert_eq!(status["pane_reported_by_agentctl"], true);
        assert_eq!(status["custom_process_identity"]["pid"], 4242);
        let commands = fixture.client.slot_commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].0.starts_with("relay:codex:"));
        let argv = fs::read_to_string(fixture.root.join("argv-root")).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        let separator = argv.iter().position(|item| *item == "--").unwrap();
        assert_eq!(argv[separator + 1], "/opt/bin/codex");
        let record: AgentRecord = serde_json::from_value(
            agent::read_private_json(&fixture.root.join("registry/worker/agent.json")).unwrap(),
        )
        .unwrap();
        record
            .validate_loaded(&fixture.root.join("registry/worker/agent.json"), "worker")
            .unwrap();
    }

    /// A stand-in `wrkslots` that records its argv and cwd, then answers like
    /// `shell-command --format json` (or fails, when told to).
    fn fake_wrkslots(root: &Path, slot_path: &Path, behaviour: &str) -> PathBuf {
        let script = root.join(format!("fake-wrkslots-{behaviour}"));
        let body = match behaviour {
            "ok" => format!(
                "printf '{{\"command\": \"exec boxed-shell\", \"slot_path\": \"{}\"}}\\n'",
                slot_path.display()
            ),
            "fail" => "echo 'unknown slot s9' >&2; exit 3".to_owned(),
            "root" => format!(
                "printf '{{\"command\": \"exec boxed-shell\", \"slot_path\": \"{}\", \"isolation\": \"root\"}}\\n'",
                slot_path.display()
            ),
            _ => "echo 'not json'".to_owned(),
        };
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$@\" > '{}'\n{body}\n",
                root.join(format!("argv-{behaviour}")).display()
            ),
        );
        script
    }

    /// Write `contents` to `path` as an executable script through a short-lived `sh`, so that no
    /// descriptor open for writing to it ever exists in this process. A script that a test writes
    /// itself and runs at once can fail with "Text file busy": a child that another test's thread
    /// forks while the file is open keeps a copy of that descriptor until the child executes.
    fn write_executable(path: &Path, contents: &str) {
        use std::io::Write;
        let mut writer = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("cat > \"$1\" && chmod 755 \"$1\"")
            .arg("sh")
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("start the script writer");
        writer
            .stdin
            .take()
            .expect("writer stdin")
            .write_all(contents.as_bytes())
            .expect("send the script");
        let status = writer.wait().expect("wait for the script writer");
        assert!(status.success(), "could not write {}", path.display());
    }

    #[test]
    fn slot_start_boxes_the_pane_and_works_in_the_slot() {
        let fixture = Fixture::new();
        // The fake Herdr reports every pane at the fixture root, so the slot is there.
        let slot_path = fixture.root.clone();
        let project = fixture.root.join("project");
        fs::create_dir_all(&project).unwrap();
        let executable = fake_wrkslots(&fixture.root, &slot_path, "ok");
        let status = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    slot: Some(SlotLaunch {
                        slot: "s1".to_owned(),
                        isolation: Some("cgroup".to_owned()),
                        project: Some(project.clone()),
                        executable: Some(executable),
                        ..SlotLaunch::default()
                    }),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        let commands = fixture.client.slot_commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].1, "exec boxed-shell");
        let canonical_slot = fs::canonicalize(&slot_path).unwrap();
        assert_eq!(status["cwd"], json!(canonical_slot.display().to_string()));
        assert_eq!(status["slot"], "s1");
        assert_eq!(
            status["slot_project"],
            fs::canonicalize(&project).unwrap().display().to_string()
        );
        assert_eq!(status["slot_isolation"], "cgroup");
        let argv = fs::read_to_string(fixture.root.join("argv-ok")).unwrap();
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(
            lines[1..],
            [
                "--project-root",
                project.to_str().unwrap(),
                "shell-command",
                "s1",
                "--isolation",
                "cgroup",
                "--format",
                "json"
            ]
        );
        assert_eq!(
            fs::canonicalize(lines[0]).unwrap(),
            fs::canonicalize(&project).unwrap()
        );
    }

    #[test]
    fn project_box_start_asks_for_a_coordinator_box_in_the_agent_cwd() {
        let fixture = Fixture::new();
        // The fake Herdr reports every pane at the fixture root: the box cwd is there.
        let executable = fake_wrkslots(&fixture.root, &fixture.root, "ok");
        let status = fixture
            .manager()
            .start(
                "planner",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    slot: Some(SlotLaunch {
                        slot: "planner".to_owned(),
                        isolation: Some("userns".to_owned()),
                        project_box: true,
                        box_writable: Some("project".to_owned()),
                        executable: Some(executable),
                        ..SlotLaunch::default()
                    }),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert_eq!(status["lifecycle"], "running");
        let commands = fixture.client.slot_commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].1, "exec boxed-shell");
        assert_eq!(status["slot"], Value::Null);
        assert_eq!(status["project_box"]["name"], "planner");
        assert_eq!(
            status["project_box"]["project"],
            fixture.root.display().to_string()
        );
        assert_eq!(status["project_box"]["isolation"], "userns");
        assert_eq!(status["project_box"]["writable"], "project");
        let argv = fs::read_to_string(fixture.root.join("argv-ok")).unwrap();
        assert_eq!(
            argv.lines().skip(1).collect::<Vec<_>>(),
            [
                "shell-command",
                "--box",
                "--name",
                "planner",
                "--cwd",
                fixture.root.to_str().unwrap(),
                "--writable",
                "project",
                "--isolation",
                "userns",
                "--format",
                "json"
            ]
        );
    }

    #[test]
    fn root_project_box_runs_the_harness_behind_the_relay() {
        let fixture = Fixture::new();
        let executable = fake_wrkslots(&fixture.root, &fixture.root, "root");
        let status = fixture
            .manager()
            .start(
                "planner",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    slot: Some(SlotLaunch {
                        slot: "planner".to_owned(),
                        project_box: true,
                        executable: Some(executable),
                        ..SlotLaunch::default()
                    }),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert_eq!(status["adapter"], "herdr-relay");
        let argv = fs::read_to_string(fixture.root.join("argv-root")).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(argv[1..4], ["shell-command", "--box", "--name"]);
        let separator = argv.iter().position(|item| *item == "--").unwrap();
        assert_eq!(argv[separator + 1], "/opt/bin/codex");
        let error = slot_shell_command(
            &SlotLaunch {
                slot: "planner".to_owned(),
                project_box: true,
                box_writable: Some("everything".to_owned()),
                executable: Some(fixture.root.join("fake-wrkslots-root")),
                ..SlotLaunch::default()
            },
            &fixture.root,
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("--box-writable"));
    }

    #[test]
    fn slot_start_defaults_to_configured_isolation_and_the_agent_cwd() {
        let fixture = Fixture::new();
        let slot_path = fixture.root.join("slots/s2");
        fs::create_dir_all(&slot_path).unwrap();
        let executable = fake_wrkslots(&fixture.root, &slot_path, "ok");
        let (line, path, isolation) = slot_shell_command(
            &SlotLaunch {
                slot: "s2".to_owned(),
                executable: Some(executable),
                ..SlotLaunch::default()
            },
            &fixture.root,
            &[],
        )
        .unwrap();
        assert_eq!(line, "exec boxed-shell");
        assert_eq!(path, slot_path);
        assert_eq!(isolation, "userns");
        let argv = fs::read_to_string(fixture.root.join("argv-ok")).unwrap();
        assert_eq!(
            argv.lines().skip(1).collect::<Vec<_>>(),
            ["shell-command", "s2", "--format", "json"]
        );
    }

    #[test]
    fn slot_failures_are_refused_before_registry_or_tab_creation() {
        let fixture = Fixture::new();
        let slot_path = fixture.root.join("slots/s9");
        fs::create_dir_all(&slot_path).unwrap();
        let cases = [
            (
                "fail",
                None,
                "wrkslots shell-command 's9' failed: unknown slot s9",
            ),
            (
                "junk",
                None,
                "wrkslots shell-command 's9' returned invalid JSON",
            ),
            (
                "ok",
                Some("namespace"),
                "--slot-isolation must be userns, cgroup, or root",
            ),
            (
                "root",
                None,
                "--slot with root isolation supports claude, codex, not \"muse\"",
            ),
        ];
        for (behaviour, isolation, expected) in cases {
            let executable = fake_wrkslots(&fixture.root, &slot_path, behaviour);
            let harness = if behaviour == "root" { "muse" } else { "codex" };
            let error = fixture
                .manager()
                .start(
                    "worker",
                    &fixture.root,
                    StartOptions {
                        harness: harness.to_owned(),
                        slot: Some(SlotLaunch {
                            slot: "s9".to_owned(),
                            isolation: isolation.map(str::to_owned),
                            executable: Some(executable),
                            ..SlotLaunch::default()
                        }),
                        ..StartOptions::default()
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{behaviour}: {error}");
        }
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    harness: "agentcloud".to_owned(),
                    slot: Some(SlotLaunch {
                        slot: "s9".to_owned(),
                        ..SlotLaunch::default()
                    }),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("--slot does not apply to harness agentcloud"));
        assert!(!fixture.root.join("registry").exists());
        assert!(fixture.client.slot_commands.lock().unwrap().is_empty());
        assert!(fixture.client.environments.lock().unwrap().is_empty());
    }

    #[test]
    fn interactive_muse_refuses_raw_duplicate_structured_options_before_allocation() {
        let fixture = Fixture::new();
        let model_error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    harness: "muse".to_owned(),
                    model: Some("structured".to_owned()),
                    harness_args: vec!["--model".to_owned(), "raw".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(model_error.to_string().contains("model field"));
        let effort_error = fixture
            .manager()
            .start_with_reasoning_effort(
                "worker",
                &fixture.root,
                "ultra",
                StartOptions {
                    harness: "muse".to_owned(),
                    harness_args: vec!["--reasoning-effort=low".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(effort_error.to_string().contains("reasoning effort"));
        assert!(!fixture.root.join("registry").exists());
        assert!(!fixture.client.started.load(Ordering::Relaxed));
        assert!(fixture.client.environments.lock().unwrap().is_empty());
    }

    #[test]
    fn direct_start_preserves_unstructured_raw_policy_and_nonoverriding_codex_config() {
        let unstructured = Fixture::new();
        let unstructured_status = unstructured
            .manager()
            .start(
                "worker",
                &unstructured.root,
                StartOptions {
                    harness_args: vec!["--reasoning-effort=high".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            unstructured_status["arguments"],
            json!(["--no-alt-screen", "--reasoning-effort=high"])
        );
        assert_eq!(unstructured_status["workspace_id"], "workspace");

        let codex = Fixture::new();
        let codex_status = codex
            .manager()
            .start(
                "worker",
                &codex.root,
                StartOptions {
                    model: Some("structured".to_owned()),
                    harness_args: vec!["-c".to_owned(), "sandbox_mode=read-only".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            codex_status["arguments"],
            json!([
                "--no-alt-screen",
                "--model",
                "structured",
                "-c",
                "sandbox_mode=read-only"
            ])
        );
        assert_eq!(codex_status["workspace_id"], "workspace");
    }

    #[test]
    fn direct_start_ignores_a_conflicting_real_herdr_workspace() {
        // Re-run a start fixture with a conflicting workspace in the real process environment.
        // The child fixture asserts the explicitly selected fake workspace, so accidentally
        // inheriting HERDR_WORKSPACE_ID changes the result and fails this parent test.
        let output = std::process::Command::new(
            std::env::current_exe().expect("resolve current test executable"),
        )
        .arg("--exact")
        .arg("subagents::tests::direct_start_preserves_unstructured_raw_policy_and_nonoverriding_codex_config")
        .arg("--test-threads=1")
        .env("HERDR_WORKSPACE_ID", "conflicting-workspace")
        .output()
        .expect("run start fixture under a conflicting workspace");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    #[test]
    fn direct_start_prefers_named_workspace_then_inherited_workspace() {
        let named = Fixture::new();
        named
            .manager()
            .with_inherited_workspace(Some("elsewhere"))
            .start(
                "worker",
                &named.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap();

        let inherited = Fixture::new();
        let status = inherited
            .manager()
            .with_inherited_workspace(Some("elsewhere"))
            .start("worker", &inherited.root, StartOptions::default())
            .unwrap();
        assert_eq!(status["workspace_id"], "elsewhere");
        let record: Value = serde_json::from_slice(
            &fs::read(inherited.root.join("registry/worker/agent.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(record["workspace_id"], "elsewhere");
        assert_eq!(
            inherited.client.panes.lock().unwrap()[0].workspace_id,
            "elsewhere"
        );
    }

    #[test]
    fn project_workspace_selects_the_named_destination_and_rejects_an_override() {
        let selected = Fixture::new();
        let status = selected
            .manager()
            .with_inherited_workspace(Some("workspace"))
            .with_project_workspace(Some("project-agents"))
            .start("worker", &selected.root, StartOptions::default())
            .unwrap();
        assert_eq!(status["workspace_id"], "project-workspace");
        assert_eq!(status["agent_status"], "idle");
        assert_eq!(
            selected.client.panes.lock().unwrap()[0].workspace_id,
            "project-workspace"
        );

        let mismatched = Fixture::new();
        let error = mismatched
            .manager()
            .with_project_workspace(Some("project-agents"))
            .start(
                "worker",
                &mismatched.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("project configuration requires \"project-agents\""));
        assert!(!mismatched.client.started.load(Ordering::Relaxed));
        assert!(mismatched.client.panes.lock().unwrap().is_empty());
    }

    #[test]
    fn managed_legacy_workspace_binding_is_accepted_without_rewrite() {
        for project_workspace in [None, Some("project-agents")] {
            let fixture = Fixture::new();
            fixture
                .client
                .report_session
                .store(false, Ordering::Relaxed);
            let manager = fixture.manager().with_project_workspace(project_workspace);
            manager
                .start("worker", &fixture.root, StartOptions::default())
                .unwrap();
            manager
                .send("worker", "create binding", DrainOptions::default())
                .unwrap();
            let path = fixture.root.join("registry/worker/queue/target.json");
            let mut document = agent::read_private_json(&path).unwrap();
            document["expected_workspace"] = json!("legacy-project");
            agent::atomic_json(&path, &document).unwrap();
            let before = fs::read(&path).unwrap();

            assert_eq!(manager.status("worker").unwrap()["agent_status"], "idle");
            manager
                .send("worker", "legacy compatible", DrainOptions::default())
                .unwrap();
            assert_eq!(
                fixture.client.runs.lock().unwrap().last().unwrap(),
                "legacy compatible"
            );
            assert_eq!(fs::read(&path).unwrap(), before);
            manager.drain("worker", DrainOptions::default()).unwrap();
            assert_eq!(fs::read(&path).unwrap(), before);
            manager
                .goal(
                    "worker",
                    Some("legacy compatible goal"),
                    DrainOptions::default(),
                    None,
                )
                .unwrap();
            assert_eq!(
                fixture.client.runs.lock().unwrap().last().unwrap(),
                "/goal legacy compatible goal"
            );
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn move_preserves_the_managed_process_and_commits_the_new_herdr_identity() {
        let fixture = Fixture::new();
        fixture.start(None);
        fixture
            .manager()
            .send("worker", "establish queue binding", DrainOptions::default())
            .unwrap();
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));

        let before_runs = fixture.client.runs.lock().unwrap().len();
        let moved = manager.move_to_project_workspace("worker").unwrap();
        assert_eq!(moved["moved"], true);
        assert_eq!(moved["recovered"], false);
        assert_eq!(moved["previous_pane_id"], "owned");
        assert_eq!(moved["pane_id"], "project-pane");
        assert_eq!(moved["tab_id"], "project-tab");
        assert_eq!(moved["workspace_id"], "project-workspace");
        assert_eq!(moved["agent_status"], "idle");
        assert_eq!(
            fixture.client.moves.lock().unwrap().as_slice(),
            &[(
                "owned".to_owned(),
                "project-workspace".to_owned(),
                "worker".to_owned(),
            )]
        );
        assert_eq!(fixture.client.runs.lock().unwrap().len(), before_runs);

        let stored = manager.load("worker").unwrap();
        assert_eq!(stored.pane_id.as_deref(), Some("project-pane"));
        assert_eq!(stored.tab_id.as_deref(), Some("project-tab"));
        assert_eq!(stored.workspace_id.as_deref(), Some("project-workspace"));
        assert_eq!(manager.read("worker", 10).unwrap(), "visible output");

        let already_there = manager.move_to_project_workspace("worker").unwrap();
        assert_eq!(already_there["moved"], false);
        assert_eq!(fixture.client.moves.lock().unwrap().len(), 1);
    }

    #[test]
    fn move_recovery_requires_intent_and_restores_delivery() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture.start(None);
        fixture
            .manager()
            .send("worker", "before move", DrainOptions::default())
            .unwrap();
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        let record = manager.load("worker").unwrap();
        fixture
            .client
            .move_pane_to_new_tab("owned", "project-workspace", "worker")
            .unwrap();
        let error = manager.move_to_project_workspace("worker").unwrap_err();
        assert!(error.to_string().contains("no durable move intent"));
        manager
            .write_move_intent(&record, "project-workspace")
            .unwrap();
        let pending = manager.status("worker").unwrap();
        assert_eq!(pending["move_pending"], true);
        assert_eq!(
            pending["probe_error"],
            "move of \"worker\" is incomplete; rerun `agentctl move worker`"
        );
        assert!(manager
            .stop("worker")
            .unwrap_err()
            .to_string()
            .contains("move is incomplete"));
        let mut replacement = record.target().unwrap();
        replacement.pane_id = Some("project-pane".to_owned());
        agent::rebind_queue(
            &fixture.root.join("registry/worker/queue"),
            &record.target().unwrap(),
            &replacement,
        )
        .unwrap();
        let recovered = manager.move_to_project_workspace("worker").unwrap();
        assert_eq!(recovered["recovered"], true);
        manager
            .send("worker", "after recovery", DrainOptions::default())
            .unwrap();
        assert_eq!(
            fixture
                .client
                .runs
                .lock()
                .unwrap()
                .last()
                .map(String::as_str),
            Some("after recovery")
        );
        assert!(!fixture.root.join("registry/worker/move.json").exists());
    }

    #[test]
    fn stale_completed_move_intent_is_row_local_and_rerun_clears_it() {
        let fixture = Fixture::new();
        fixture.start(None);
        let before = fixture.manager().load("worker").unwrap();
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        manager.move_to_project_workspace("worker").unwrap();
        manager
            .write_move_intent(&before, "project-workspace")
            .unwrap();

        let status = manager.status("worker").unwrap();
        assert_eq!(status["move_pending"], true);
        assert_eq!(
            status["probe_error"],
            "move of \"worker\" is incomplete; rerun `agentctl move worker`"
        );
        let rows = manager.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "worker");
        assert!(rows[0]["probe_error"]
            .as_str()
            .unwrap()
            .contains("rerun `agentctl move worker`"));

        let repeated = manager.move_to_project_workspace("worker").unwrap();
        assert_eq!(repeated["moved"], false);
        assert!(!fixture.root.join("registry/worker/move.json").exists());

        agent::atomic_json(&fixture.root.join("registry/worker/move.json"), &json!({})).unwrap();
        let invalid = manager.status("worker").unwrap();
        assert_eq!(invalid["agent_status"], "unknown");
        assert!(invalid["probe_error"]
            .as_str()
            .unwrap()
            .contains("rerun `agentctl move worker`"));
        assert_eq!(manager.list().unwrap().len(), 1);
    }

    #[test]
    fn pending_move_can_finish_after_workspace_policy_is_removed() {
        let fixture = Fixture::new();
        fixture.start(None);
        let record = fixture.manager().load("worker").unwrap();
        fixture
            .manager()
            .with_project_workspace(Some("project-agents"))
            .write_move_intent(&record, "project-workspace")
            .unwrap();
        let manager = fixture.manager();

        let moved = manager.move_to_project_workspace("worker").unwrap();
        assert_eq!(moved["workspace_id"], "project-workspace");
        assert_eq!(
            fixture.client.moves.lock().unwrap().as_slice(),
            &[(
                "owned".to_owned(),
                "project-workspace".to_owned(),
                "worker".to_owned(),
            )]
        );
        assert!(!fixture.root.join("registry/worker/move.json").exists());
        assert_eq!(manager.stop("worker").unwrap()["pane_closed"], true);
    }

    #[test]
    fn move_preflights_foreign_binding_before_herdr_and_preserves_queue_states() {
        let fixture = Fixture::new();
        fixture.start(None);
        fixture
            .manager()
            .send("worker", "bind", DrainOptions::default())
            .unwrap();
        let queue = fixture.root.join("registry/worker/queue");
        agent::atomic_json(
            &queue.join("target.json"),
            &json!({
                "kind": "pane", "pane_id": "foreign", "expected_agent": "codex",
                "expected_workspace": null, "expected_cwd": fixture.root,
            }),
        )
        .unwrap();
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        let error = manager.move_to_project_workspace("worker").unwrap_err();
        assert!(error.to_string().contains("refusing move"));
        assert!(fixture.client.moves.lock().unwrap().is_empty());

        fs::remove_file(queue.join("target.json")).unwrap();
        fixture
            .manager()
            .send("worker", "restore", DrainOptions::default())
            .unwrap();
        for state in ["inbox", "inflight", "processed", "failed"] {
            let directory = queue.join(state);
            fs::create_dir_all(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            agent::atomic_json(&directory.join(format!("{state}.json")), &json!({})).unwrap();
        }
        manager.move_to_project_workspace("worker").unwrap();
        for state in ["inbox", "inflight", "processed", "failed"] {
            assert!(queue.join(state).join(format!("{state}.json")).exists());
        }
    }

    #[test]
    fn pane_binding_moves_and_delivers_to_replacement() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture.start(None);
        fixture
            .manager()
            .send("worker", "bind pane", DrainOptions::default())
            .unwrap();
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        manager.move_to_project_workspace("worker").unwrap();
        manager
            .send("worker", "replacement pane", DrainOptions::default())
            .unwrap();
        assert_eq!(
            fixture
                .client
                .runs
                .lock()
                .unwrap()
                .last()
                .map(String::as_str),
            Some("replacement pane")
        );
    }

    #[test]
    fn move_refuses_sibling_tab_before_touching_herdr() {
        let fixture = Fixture::new();
        fixture.start(None);
        fixture
            .client
            .panes
            .lock()
            .unwrap()
            .push(Fake::pane("sibling"));
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        let error = manager.move_to_project_workspace("worker").unwrap_err();
        assert!(error.to_string().contains("tab contains another pane"));
        assert!(fixture.client.moves.lock().unwrap().is_empty());
    }

    #[test]
    fn move_refuses_name_and_final_presentation_mismatches_with_intent_retained() {
        let name = Fixture::new();
        name.start(None);
        name.client
            .move_name_follows
            .store(false, Ordering::Relaxed);
        let name_manager = name
            .manager()
            .with_project_workspace(Some("project-agents"));
        let error = name_manager
            .move_to_project_workspace("worker")
            .unwrap_err();
        assert!(error.to_string().contains("managed name did not follow"));
        assert!(name.root.join("registry/worker/move.json").exists());

        let presentation = Fixture::new();
        presentation.start(None);
        presentation
            .client
            .move_return_matches
            .store(false, Ordering::Relaxed);
        let presentation_manager = presentation
            .manager()
            .with_project_workspace(Some("project-agents"));
        let error = presentation_manager
            .move_to_project_workspace("worker")
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("final presentation verification failed"));
        assert!(presentation.root.join("registry/worker/move.json").exists());
    }

    #[test]
    fn failed_pre_herdr_move_clears_intent_and_cannot_authorize_recovery() {
        let fixture = Fixture::new();
        fixture.start(None);
        fixture
            .client
            .move_fails_before_change
            .store(true, Ordering::Relaxed);
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        let error = manager.move_to_project_workspace("worker").unwrap_err();
        assert!(error.to_string().contains("before mutation"));
        assert!(!fixture.root.join("registry/worker/move.json").exists());

        fixture
            .client
            .move_fails_before_change
            .store(false, Ordering::Relaxed);
        *fixture.client.panes.lock().unwrap() = vec![Pane {
            pane_id: "replacement".to_owned(),
            tab_id: "other".to_owned(),
            workspace_id: "project-workspace".to_owned(),
        }];
        *fixture.client.named_pane.lock().unwrap() = "replacement".to_owned();
        assert!(manager
            .move_to_project_workspace("worker")
            .unwrap_err()
            .to_string()
            .contains("no durable move intent"));
    }

    #[test]
    fn move_refuses_false_recovery_and_unsupported_records() {
        let both = Fixture::new();
        both.start(None);
        let both_manager = both
            .manager()
            .with_project_workspace(Some("project-agents"));
        let record = both_manager.load("worker").unwrap();
        both_manager
            .write_move_intent(&record, "project-workspace")
            .unwrap();
        both.client
            .panes
            .lock()
            .unwrap()
            .push(Fake::pane("replacement"));
        *both.client.named_pane.lock().unwrap() = "replacement".to_owned();
        let error = both_manager
            .move_to_project_workspace("worker")
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("both recorded and named panes are live"));

        let no_policy = Fixture::new();
        no_policy.start(None);
        assert!(no_policy
            .manager()
            .move_to_project_workspace("worker")
            .unwrap_err()
            .to_string()
            .contains("requires a workspace field"));

        let adopted = Fixture::new();
        adopted.adopt();
        assert!(adopted
            .manager()
            .with_project_workspace(Some("project-agents"))
            .move_to_project_workspace("foreign")
            .unwrap_err()
            .to_string()
            .contains("supports only"));

        let custom = Fixture::new();
        custom
            .manager()
            .start(
                "worker",
                &custom.root,
                StartOptions {
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        assert!(custom
            .manager()
            .with_project_workspace(Some("project-agents"))
            .move_to_project_workspace("worker")
            .unwrap_err()
            .to_string()
            .contains("supports only"));

        let mismatch = Fixture::new();
        let error = mismatch
            .manager()
            .with_project_workspace(Some("project-agents"))
            .adopt("foreign", mismatch.adopt_options())
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not match project workspace"));
    }

    #[test]
    fn policy_never_blocks_status_stop_or_workspace_identity_guard() {
        let fixture = Fixture::new();
        fixture.start(None);
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        assert_eq!(manager.status("worker").unwrap()["agent_status"], "idle");
        assert_eq!(manager.stop("worker").unwrap()["pane_closed"], true);

        let adopted = Fixture::new();
        adopted.adopt();
        let adopted_manager = adopted
            .manager()
            .with_project_workspace(Some("project-agents"));
        assert_eq!(
            adopted_manager.stop("foreign").unwrap()["runtime_preserved"],
            true
        );

        let changed = Fixture::new();
        changed.start(None);
        changed
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let mut changed_record = changed.manager().load("worker").unwrap();
        changed_record.session_agent = None;
        changed_record.session_value = None;
        changed.manager().save(&changed_record).unwrap();
        changed.client.panes.lock().unwrap()[0].workspace_id = "elsewhere".to_owned();
        let error = changed.manager().read("worker", 10).unwrap_err();
        assert!(error.to_string().contains("workspace identity changed"));
        let error = changed
            .manager()
            .send("worker", "must refuse", DrainOptions::default())
            .unwrap_err();
        assert!(error
            .to_string()
            .ends_with(": agent \"worker\" workspace identity changed"));
    }

    #[test]
    fn goal_replacement_wait_checks_non_runtime_workspace_identity() {
        let fixture = Fixture::new();
        fixture.start(None);
        let record = fixture.manager().load("worker").unwrap();
        fixture
            .client
            .fail_first_wait
            .store(true, Ordering::Relaxed);
        fixture.client.panes.lock().unwrap()[0].workspace_id = "elsewhere".to_owned();
        let client = WorkspaceClient {
            client: &fixture.client,
            record: &record,
            goal_objective: Mutex::new(Some("replace the active objective".to_owned())),
            custom_submission: Mutex::new(None),
            queue: None,
            check_prompt: false,
            expected_workspace: None,
            readback: None,
        };

        let error = AgentApi::wait_agent_status(&client, "owned", "working", 30).unwrap_err();
        assert_eq!(
            error.to_string(),
            "agent \"worker\" workspace identity changed"
        );
    }

    #[test]
    fn policy_changes_never_become_queue_identity() {
        let fixture = Fixture::new();
        fixture.start(None);
        fixture
            .manager()
            .send("worker", "before policy", DrainOptions::default())
            .unwrap();
        fixture
            .manager()
            .with_project_workspace(Some("project-agents"))
            .move_to_project_workspace("worker")
            .unwrap();
        fixture
            .manager()
            .send("worker", "stale manager", DrainOptions::default())
            .unwrap();
        assert_eq!(
            fixture
                .client
                .runs
                .lock()
                .unwrap()
                .last()
                .map(String::as_str),
            Some("stale manager")
        );

        let adopted = Fixture::new();
        adopted.prepare_foreign();
        adopted.client.panes.lock().unwrap()[0].workspace_id = "project-workspace".to_owned();
        let mut options = adopted.adopt_options();
        options.expected_workspace = "project-agents".to_owned();
        let manager = adopted
            .manager()
            .with_project_workspace(Some("project-agents"));
        manager.adopt("foreign", options).unwrap();
        manager
            .send("foreign", "adopted in place", DrainOptions::default())
            .unwrap();
        assert_eq!(
            adopted
                .client
                .runs
                .lock()
                .unwrap()
                .last()
                .map(String::as_str),
            Some("adopted in place")
        );
    }

    #[test]
    fn project_policy_blocks_control_until_a_misplaced_agent_is_moved() {
        let fixture = Fixture::new();
        fixture.start(None);
        let manager = fixture
            .manager()
            .with_project_workspace(Some("project-agents"));
        let error = manager.read("worker", 10).unwrap_err();
        assert_eq!(
            error.to_string(),
            "refusing pane owned: workspace is \"subagents\", expected \"project-agents\""
        );
        assert!(fixture.client.moves.lock().unwrap().is_empty());
    }

    #[test]
    fn direct_start_rejects_quoted_config_and_opaque_profile_conflicts_before_allocation() {
        let fixture = Fixture::new();
        let model_error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    model: Some("structured".to_owned()),
                    harness_args: vec!["--config=\"model\"=\"raw\"".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(model_error.to_string().contains("model field"));
        let effort_error = fixture
            .manager()
            .start_with_reasoning_effort(
                "worker",
                &fixture.root,
                "ultra",
                StartOptions {
                    harness_args: vec!["-c\"model_reasoning_effort\"=\"low\"".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(effort_error.to_string().contains("reasoning effort"));
        let profile_model_error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    model: Some("structured".to_owned()),
                    harness_args: vec!["--profile".to_owned(), "attacker".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(profile_model_error
            .to_string()
            .contains("raw Codex profile"));
        let profile_effort_error = fixture
            .manager()
            .start_with_reasoning_effort(
                "worker",
                &fixture.root,
                "ultra",
                StartOptions {
                    harness_args: vec!["-pattacker".to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(profile_effort_error
            .to_string()
            .contains("raw Codex profile"));
        for arguments in [
            vec!["-c".to_owned(), "profile=attacker".to_owned()],
            vec!["--config=profile=attacker".to_owned()],
            vec!["-c\"profile\"=\"attacker\"".to_owned()],
        ] {
            let error = fixture
                .manager()
                .start(
                    "worker",
                    &fixture.root,
                    StartOptions {
                        model: Some("structured".to_owned()),
                        harness_args: arguments,
                        ..StartOptions::default()
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains("raw Codex profile"));
        }
        assert!(!fixture.root.join("registry").exists());
        assert!(!fixture.client.started.load(Ordering::Relaxed));
        assert!(fixture.client.environments.lock().unwrap().is_empty());
    }

    #[test]
    fn herdr_093_muse_adoption_refuses_a_wrong_reported_harness_without_registration() {
        let fixture = Fixture::new();
        fixture.prepare_foreign();
        let mut options = fixture.adopt_options();
        options.harness = "muse".to_owned();
        let error = fixture.manager().adopt("foreign", options).unwrap_err();
        assert!(
            error.to_string().contains("expected 'muse'")
                || error.to_string().contains("expected \"muse\""),
            "{error}"
        );
        assert!(!fixture.root.join("registry/foreign").exists());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(fixture.client.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_launch_status_does_not_retain_environment_values() {
        let fixture = Fixture::new();
        let secret = "ACCESS_TOKEN=literal-sensitive-value";
        fixture.client.fail_panes.store(true, Ordering::Relaxed);
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    environment: vec![secret.to_owned()],
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("pane query failed"));
        let record = fixture.manager().load("worker").unwrap();
        assert_eq!(record.lifecycle, "launch_failed");
        assert_eq!(
            record.error.as_deref(),
            Some("launch failed with caller-supplied environment; details omitted from status")
        );
        assert!(!serde_json::to_string(&record).unwrap().contains(secret));
    }

    #[test]
    fn start_session_change_leaves_launch_failed_record_stoppable() {
        let fixture = Fixture::new();
        fixture
            .client
            .change_owned_session_after_save
            .store(true, Ordering::Relaxed);
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("session"), "{error}");
        let failed = fixture.manager().load("worker").unwrap();
        assert_eq!(failed.lifecycle, "launch_failed");
        assert!(failed.session_agent.is_none() && failed.session_value.is_none());
        assert_eq!(
            fixture.manager().stop("worker").unwrap()["pane_closed"],
            true
        );
    }

    #[test]
    fn adopted_agent_keeps_native_identity_and_uses_the_durable_named_interface() {
        let fixture = Fixture::new();
        let adopted = fixture.adopt();
        let manager = fixture.manager();
        assert_eq!(adopted["adapter"], "herdr-foreign");
        assert_eq!(adopted["pane_id"], "owned");
        assert_eq!(adopted["session_agent"], "codex");
        assert_eq!(adopted["session_value"], "thread");
        assert_eq!(
            adopted["foreign_shell_identity"],
            json!(Fake::foreign_shell_identity())
        );
        assert_eq!(manager.list().unwrap().len(), 1);
        manager
            .send("foreign", "follow up", DrainOptions::default())
            .unwrap();
        assert_eq!(manager.read("foreign", 10).unwrap(), "visible output");
        // Just after a confirmed delivery, an idle pane is not yet readiness.
        let early = manager
            .wait("foreign", Duration::from_secs(0))
            .unwrap_err()
            .to_string();
        assert!(early.contains("has not been seen working"), "{early}");
        let later = || unix_seconds() + DELIVERY_SETTLE_SECONDS + 1.0;
        assert_eq!(
            manager
                .wait_at("foreign", Duration::from_secs(0), &later)
                .unwrap()["agent_status"],
            "idle"
        );
        assert_eq!(
            manager.bind_session("foreign", "thread", None).unwrap()["source"],
            "explicit"
        );
        let command = ["/bin/false".to_owned()];
        let goal = manager
            .goal(
                "foreign",
                Some("finish adopted work"),
                DrainOptions::default(),
                Some(&command),
            )
            .unwrap();
        assert_eq!(goal["delivery"], "delivered");
        assert_eq!(
            *fixture.client.runs.lock().unwrap(),
            ["follow up", "/goal finish adopted work"]
        );
        let binding =
            agent::read_private_json(&fixture.root.join("registry/foreign/queue/target.json"))
                .unwrap();
        assert_eq!(binding["kind"], "session");
        assert_eq!(binding["agent"], "codex");
        assert_eq!(binding["value"], "thread");
    }

    #[test]
    fn stopping_an_adopted_agent_only_unregisters_and_archives_control_state() {
        let fixture = Fixture::new();
        let adopted = fixture.adopt();
        let manager = fixture.manager();
        manager
            .send("foreign", "retained request", DrainOptions::default())
            .unwrap();
        let presentation = Fake::pane("owned");

        let stopped = manager.stop("foreign").unwrap();

        assert_eq!(stopped["runtime_preserved"], true);
        assert_eq!(stopped["pane_closed"], false);
        assert_eq!(stopped["tab_closed"], false);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(*fixture.client.panes.lock().unwrap(), [presentation]);
        let archive = PathBuf::from(stopped["archive"].as_str().unwrap());
        let saved = agent::read_private_json(&archive.join("agent.json")).unwrap();
        assert_eq!(saved["adapter"], "herdr-foreign");
        assert_eq!(saved["token"], adopted["token"]);
        assert!(archive.join("output.json").is_file());
        assert_eq!(
            fs::read_dir(archive.join("queue/processed"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn adoption_archives_a_failed_generation_if_the_shell_changes_after_save() {
        let fixture = Fixture::new();
        fixture.prepare_foreign();
        fixture
            .client
            .change_foreign_shell_after_save
            .store(true, Ordering::Relaxed);

        let error = fixture
            .manager()
            .adopt("foreign", fixture.adopt_options())
            .unwrap_err();

        assert!(error.to_string().contains("was not registered"));
        assert!(!fixture.root.join("registry/foreign").exists());
        let archives = fs::read_dir(fixture.root.join("registry/archive"))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(archives.len(), 1);
        let saved = agent::read_private_json(&archives[0].path().join("agent.json")).unwrap();
        assert_eq!(saved["lifecycle"], "adopt_failed");
        assert!(saved["error"]
            .as_str()
            .unwrap()
            .contains("shell process identity changed"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn stopping_a_confirmed_dead_adopted_agent_only_archives_control_state() {
        let fixture = Fixture::new();
        let adopted = fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let presentation = Fake::pane("owned");

        let stopped = fixture.manager().stop("foreign").unwrap();

        assert_eq!(stopped["runtime_preserved"], true);
        assert_eq!(stopped["pane_closed"], false);
        assert_eq!(stopped["tab_closed"], false);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(*fixture.client.panes.lock().unwrap(), [presentation]);
        let archive = PathBuf::from(stopped["archive"].as_str().unwrap());
        let saved = agent::read_private_json(&archive.join("agent.json")).unwrap();
        assert_eq!(saved["token"], adopted["token"]);
        assert!(archive.join("output.json").is_file());
    }

    #[test]
    fn stopping_an_absent_adopted_agent_refuses_a_non_idle_pane() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture
            .client
            .custom_at_idle_shell
            .store(false, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error.to_string().contains("identity-bound idle shell"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_an_absent_adopted_agent_refuses_native_session_residue() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("absent agent has native session identity"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_an_absent_adopted_agent_refuses_replaced_shell_generations() {
        let original = Fake::foreign_shell_identity();
        let mut replacements = Vec::new();
        let mut new_pane_shell = original.clone();
        new_pane_shell.pid += 1;
        replacements.push(new_pane_shell);
        let mut reused_pid = original.clone();
        reused_pid.starttime_ticks += 1;
        replacements.push(reused_pid);
        let mut replaced_image = original;
        replaced_image.executable_inode += 1;
        replacements.push(replaced_image);

        for replacement in replacements {
            let fixture = Fixture::new();
            fixture.adopt();
            fixture.client.started.store(false, Ordering::Relaxed);
            fixture
                .client
                .report_session
                .store(false, Ordering::Relaxed);
            *fixture.client.foreign_shell_identity.lock().unwrap() = replacement;

            let error = fixture.manager().stop("foreign").unwrap_err();

            assert!(error
                .to_string()
                .contains("recorded pane shell generation changed"));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert_eq!(
                fixture.manager().load("foreign").unwrap().lifecycle,
                "running"
            );
        }
    }

    #[test]
    fn stopping_an_absent_legacy_adopted_agent_refuses_missing_shell_identity() {
        let fixture = Fixture::new();
        fixture.adopt();
        let path = fixture.root.join("registry/foreign/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .remove("foreign_shell_identity");
        agent::atomic_json(&path, &document).unwrap();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("legacy record has no identity-bound"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn explicit_legacy_recovery_preserves_record_queue_and_foreign_runtime() {
        let fixture = Fixture::new();
        let adopted = fixture.adopt();
        fixture
            .manager()
            .send("foreign", "retained request", DrainOptions::default())
            .unwrap();
        let record_path = fixture.root.join("registry/foreign/agent.json");
        let mut document = agent::read_private_json(&record_path).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .remove("foreign_shell_identity");
        agent::atomic_json(&record_path, &document).unwrap();
        let raw = fs::read(&record_path).unwrap();
        let digest = format!("{:x}", Sha256::digest(&raw));
        let queue_before = fs::read(
            fixture
                .root
                .join("registry/foreign/queue/processed")
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let presentation = Fake::pane("owned");

        let stopped = fixture
            .manager()
            .stop_with_options(
                "foreign",
                StopOptions {
                    expected_token: Some(adopted["token"].as_str().unwrap().to_owned()),
                    recover_legacy_adoption: true,
                    retire_dead_adoption: false,
                    expected_record_sha256: Some(digest.clone()),
                    skip_cloud_halt: false,
                },
            )
            .unwrap();

        let archive = PathBuf::from(stopped["archive"].as_str().unwrap());
        assert_eq!(stopped["recovered_legacy_adoption"], true);
        assert_eq!(stopped["runtime_preserved"], true);
        assert_eq!(stopped["record_sha256"], digest);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(*fixture.client.panes.lock().unwrap(), [presentation]);
        assert_eq!(fs::read(archive.join("agent.json")).unwrap(), raw);
        let archived_queue = fs::read(
            archive
                .join("queue/processed")
                .read_dir()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert_eq!(archived_queue, queue_before);
        assert!(archive.join("output.json").is_file());
    }

    #[test]
    fn explicit_legacy_recovery_requires_exact_options_and_absent_raw_identity_key() {
        for case in [
            "missing-token",
            "missing-hash",
            "wrong-token",
            "wrong-hash",
            "null",
        ] {
            let fixture = Fixture::new();
            let (token, mut digest, _) = fixture.make_legacy_dead();
            if case == "null" {
                let path = fixture.root.join("registry/foreign/agent.json");
                let mut document = agent::read_private_json(&path).unwrap();
                document["foreign_shell_identity"] = Value::Null;
                agent::atomic_json(&path, &document).unwrap();
                digest = format!("{:x}", Sha256::digest(fs::read(path).unwrap()));
            }
            let options = StopOptions {
                expected_token: (case != "missing-token").then(|| {
                    if case == "wrong-token" {
                        "wrong-generation".to_owned()
                    } else {
                        token.clone()
                    }
                }),
                recover_legacy_adoption: true,
                retire_dead_adoption: false,
                expected_record_sha256: (case != "missing-hash").then(|| {
                    if case == "wrong-hash" {
                        "0".repeat(64)
                    } else {
                        digest.clone()
                    }
                }),
                skip_cloud_halt: false,
            };
            assert!(fixture
                .manager()
                .stop_with_options("foreign", options)
                .is_err());
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.root.join("registry/foreign").is_dir());
        }
    }

    #[test]
    fn explicit_legacy_recovery_refuses_all_shell_generation_changes_during_capture() {
        let original = Fake::foreign_shell_identity();
        let mut replacements = Vec::new();
        let mut value = original.clone();
        value.pid += 1;
        replacements.push(PaneShellProof {
            identity: value,
            executable_path: PathBuf::from("/bin/bash"),
        });
        let mut value = original.clone();
        value.starttime_ticks += 1;
        replacements.push(PaneShellProof {
            identity: value,
            executable_path: PathBuf::from("/bin/bash"),
        });
        let mut value = original.clone();
        value.boot_id = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned();
        replacements.push(PaneShellProof {
            identity: value,
            executable_path: PathBuf::from("/bin/bash"),
        });
        let mut value = original.clone();
        value.executable_device += 1;
        replacements.push(PaneShellProof {
            identity: value,
            executable_path: PathBuf::from("/bin/bash"),
        });
        let mut value = original.clone();
        value.executable_inode += 1;
        replacements.push(PaneShellProof {
            identity: value,
            executable_path: PathBuf::from("/bin/bash"),
        });
        replacements.push(PaneShellProof {
            identity: original,
            executable_path: PathBuf::from("/bin/dash"),
        });

        for replacement in replacements {
            let fixture = Fixture::new();
            let (token, digest, _) = fixture.make_legacy_dead();
            *fixture.client.replacement_shell_on_read.lock().unwrap() = Some(replacement);
            let error = fixture
                .manager()
                .stop_with_options(
                    "foreign",
                    StopOptions {
                        expected_token: Some(token),
                        recover_legacy_adoption: true,
                        retire_dead_adoption: false,
                        expected_record_sha256: Some(digest),
                        skip_cloud_halt: false,
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains("runtime identity changed"));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.root.join("registry/foreign").is_dir());
        }
    }

    #[test]
    fn explicit_legacy_recovery_refuses_live_busy_session_moved_or_ambiguous_panes() {
        for case in ["live", "busy", "session", "moved", "sibling", "duplicate"] {
            let fixture = Fixture::new();
            let (token, digest, _) = fixture.make_legacy_dead();
            match case {
                "live" => fixture.client.started.store(true, Ordering::Relaxed),
                "busy" => fixture
                    .client
                    .custom_at_idle_shell
                    .store(false, Ordering::Relaxed),
                "session" => fixture.client.report_session.store(true, Ordering::Relaxed),
                "moved" => fixture.client.panes.lock().unwrap()[0].tab_id = "moved".to_owned(),
                "sibling" => fixture
                    .client
                    .panes
                    .lock()
                    .unwrap()
                    .push(Fake::pane("sibling")),
                "duplicate" => {
                    let mut duplicate = Fake::pane("owned");
                    duplicate.tab_id = "duplicate".to_owned();
                    fixture.client.panes.lock().unwrap().push(duplicate);
                }
                _ => unreachable!(),
            }
            assert!(fixture
                .manager()
                .stop_with_options(
                    "foreign",
                    StopOptions {
                        expected_token: Some(token),
                        recover_legacy_adoption: true,
                        retire_dead_adoption: false,
                        expected_record_sha256: Some(digest),
                        skip_cloud_halt: false,
                    },
                )
                .is_err());
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.root.join("registry/foreign").is_dir());
        }
    }

    #[test]
    fn explicit_legacy_recovery_refuses_record_change_or_capture_failure() {
        for case in ["record", "directory", "capture", "procfs"] {
            let fixture = Fixture::new();
            let (token, digest, _) = fixture.make_legacy_dead();
            if case == "record" {
                fixture
                    .client
                    .change_record_token_on_read
                    .store(true, Ordering::Relaxed);
            } else if case == "directory" {
                fixture
                    .client
                    .replace_record_directory_on_read
                    .store(true, Ordering::Relaxed);
            } else if case == "capture" {
                fixture.client.fail_read.store(true, Ordering::Relaxed);
            } else {
                fixture
                    .client
                    .fail_shell_proof
                    .store(true, Ordering::Relaxed);
            }
            let error = fixture
                .manager()
                .stop_with_options(
                    "foreign",
                    StopOptions {
                        expected_token: Some(token),
                        recover_legacy_adoption: true,
                        retire_dead_adoption: false,
                        expected_record_sha256: Some(digest),
                        skip_cloud_halt: false,
                    },
                )
                .unwrap_err();
            if matches!(case, "capture" | "procfs") {
                assert_eq!(error.exit_code(), 75);
            }
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.root.join("registry/foreign").is_dir());
        }
    }

    #[test]
    fn explicit_legacy_recovery_binds_absent_key_and_parsed_record_to_same_bytes() {
        let fixture = Fixture::new();
        let (token, digest, _) = fixture.make_legacy_dead();
        let output = fixture.root.join("registry/foreign/output.json");
        let prior = b"{\"prior\":\"evidence\"}\n";
        fs::write(&output, prior).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();
        fixture
            .client
            .add_null_shell_identity_on_read
            .store(true, Ordering::Relaxed);

        let error = fixture
            .manager()
            .stop_with_options(
                "foreign",
                StopOptions {
                    expected_token: Some(token),
                    recover_legacy_adoption: true,
                    retire_dead_adoption: false,
                    expected_record_sha256: Some(digest),
                    skip_cloud_halt: false,
                },
            )
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("requires foreign_shell_identity to be absent"));
        assert_eq!(fs::read(output).unwrap(), prior);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn explicit_legacy_recovery_rechecks_record_after_output_preparation() {
        let fixture = Fixture::new();
        let (token, digest, _) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let path = fixture.root.join("registry/foreign/agent.json");
        let options = StopOptions {
            expected_token: Some(token),
            recover_legacy_adoption: true,
            retire_dead_adoption: false,
            expected_record_sha256: Some(digest),
            skip_cloud_halt: false,
        };

        let error = manager
            .recover_legacy_adoption_locked_with(&record, &pinned, &options, || {
                let mut document = agent::read_private_json(&path).unwrap();
                document["foreign_shell_identity"] = Value::Null;
                agent::atomic_json(&path, &document).unwrap();
            })
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("requires foreign_shell_identity to be absent"));
        assert!(fixture.root.join("registry/foreign").is_dir());
        assert!(!fixture.root.join("registry/foreign/output.json").exists());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn explicit_legacy_recovery_refuses_oversize_serialized_output() {
        let fixture = Fixture::new();
        let (token, digest, raw) = fixture.make_legacy_dead();
        fixture.client.oversized_read.store(true, Ordering::Relaxed);

        let error = fixture
            .manager()
            .stop_with_options(
                "foreign",
                StopOptions {
                    expected_token: Some(token.clone()),
                    recover_legacy_adoption: true,
                    retire_dead_adoption: false,
                    expected_record_sha256: Some(digest),
                    skip_cloud_halt: false,
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("output.json larger"));
        assert_eq!(
            fs::read(fixture.root.join("registry/foreign/agent.json")).unwrap(),
            raw
        );
        assert!(!fixture.root.join("registry/foreign/output.json").exists());
        assert!(!fixture
            .root
            .join(format!("registry/archive/foreign-{token}"))
            .exists());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn explicit_legacy_recovery_uses_utf8_snapshot_budget() {
        let fixture = Fixture::new();
        let (token, digest, _) = fixture.make_legacy_dead();
        fixture.client.non_ascii_read.store(true, Ordering::Relaxed);
        let result = fixture
            .manager()
            .stop_with_options(
                "foreign",
                StopOptions {
                    expected_token: Some(token),
                    recover_legacy_adoption: true,
                    retire_dead_adoption: false,
                    expected_record_sha256: Some(digest),
                    skip_cloud_halt: false,
                },
            )
            .unwrap();
        let output = PathBuf::from(result["archive"].as_str().unwrap()).join("output.json");
        let bytes = fs::read(&output).unwrap();

        assert!(bytes.len() < MAX_SNAPSHOT_BYTES);
        assert_eq!(
            agent::read_private_json(&output).unwrap()["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            3 << 20
        );
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn directory_pin_refuses_validate_to_open_replacement() {
        for target_kind in ["agent", "parent"] {
            for replacement in ["private", "public", "missing"] {
                let fixture = Fixture::new();
                fixture.make_legacy_dead();
                let manager = fixture.manager();
                let target = if target_kind == "agent" {
                    fixture.root.join("registry/foreign")
                } else {
                    let path = fixture.root.join("registry/archive");
                    fs::create_dir(&path).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                    path
                };
                let displaced = target.with_file_name(format!(
                    ".{}-original",
                    target.file_name().unwrap().to_string_lossy()
                ));
                let swap = || {
                    fs::rename(&target, &displaced).unwrap();
                    if replacement != "missing" {
                        fs::create_dir(&target).unwrap();
                        let mode = if replacement == "private" {
                            0o700
                        } else {
                            0o755
                        };
                        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
                    }
                };

                let result = if target_kind == "agent" {
                    manager
                        .pinned_agent_directory_with("foreign", swap)
                        .map(|_| ())
                } else {
                    ManagedAgents::<Fake>::pinned_parent_directory_with(
                        &target,
                        "agent archive",
                        swap,
                    )
                    .map(|_| ())
                };

                assert!(result.is_err(), "{target_kind}/{replacement} was pinned");
                assert!(displaced.is_dir());
                assert_eq!(target.exists(), replacement != "missing");
            }
        }
    }

    #[test]
    fn pinned_directory_recheck_refuses_postopen_permission_change() {
        let fixture = Fixture::new();
        fixture.make_legacy_dead();
        let manager = fixture.manager();
        let agent_path = fixture.root.join("registry/foreign");
        let agent_pinned = manager.pinned_agent_directory("foreign").unwrap();
        fs::set_permissions(&agent_path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ManagedAgents::<Fake>::verify_pinned_agent_directory(&agent_pinned).is_err());

        let parent_path = fixture.root.join("registry/archive");
        fs::set_permissions(&agent_path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&parent_path).unwrap();
        fs::set_permissions(&parent_path, fs::Permissions::from_mode(0o700)).unwrap();
        let parent_pinned =
            ManagedAgents::<Fake>::pinned_parent_directory(&parent_path, "agent archive").unwrap();
        fs::set_permissions(&parent_path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ManagedAgents::<Fake>::verify_pinned_parent_directory(
            &parent_pinned,
            "agent archive",
        )
        .is_err());
    }

    #[test]
    fn atomic_snapshot_refuses_replaced_staging_and_installed_generations() {
        for phase in ["before-rename", "after-rename"] {
            let fixture = Fixture::new();
            fixture.make_legacy_dead();
            let manager = fixture.manager();
            let pinned = manager.pinned_agent_directory("foreign").unwrap();
            let active = fixture.root.join("registry/foreign");
            let held = active.join(format!(".held-{phase}"));
            let content = b"{\"new\":true}\n";
            let result = atomic_replace_bytes_with(
                &pinned,
                "output.json",
                content,
                (
                    |_pinned, temporary| {
                        if phase == "before-rename" {
                            fs::rename(active.join(temporary), &held).unwrap();
                            fs::write(active.join(temporary), vec![b'x'; content.len()]).unwrap();
                            fs::set_permissions(
                                active.join(temporary),
                                fs::Permissions::from_mode(0o600),
                            )
                            .unwrap();
                        }
                        Ok(())
                    },
                    |_pinned, name, temporary| {
                        if phase == "after-rename" {
                            fs::rename(active.join(name), &held).unwrap();
                            fs::write(active.join(name), b"replacement").unwrap();
                            fs::set_permissions(
                                active.join(name),
                                fs::Permissions::from_mode(0o600),
                            )
                            .unwrap();
                            fs::write(active.join(temporary), b"new-temp-owner").unwrap();
                            fs::set_permissions(
                                active.join(temporary),
                                fs::Permissions::from_mode(0o600),
                            )
                            .unwrap();
                        }
                        Ok(())
                    },
                ),
            );
            let error = result.unwrap_err().to_string();

            assert!(held.is_file());
            assert_eq!(fs::read(&held).unwrap(), content);
            if phase == "before-rename" {
                assert!(error.contains("staging generation changed"));
                assert!(error.contains("replacement was preserved"));
                assert!(!active.join("output.json").exists());
                let replacement = fs::read_dir(&active)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".output.json-recovery-")
                    })
                    .unwrap();
                assert_eq!(fs::read(replacement).unwrap(), vec![b'x'; content.len()]);
            } else {
                assert!(error.contains("was not the staged generation"));
                assert_eq!(
                    fs::read(active.join("output.json")).unwrap(),
                    b"replacement"
                );
                assert!(fs::read_dir(&active).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".output.json-recovery-")
                }));
            }
        }
    }

    #[test]
    fn legacy_recovery_never_restores_over_replaced_artifact_generations() {
        for phase in ["before-rename", "after-rename"] {
            let fixture = Fixture::new();
            let (_token, _digest, raw) = fixture.make_legacy_dead();
            let manager = fixture.manager();
            let record = manager.load("foreign").unwrap();
            let (_archive, destination) = manager.archive_destination(&record).unwrap();
            let pinned = manager.pinned_agent_directory("foreign").unwrap();
            let active = fixture.root.join("registry/foreign");
            let prior = b"{\"prior\":true}\n";
            fs::write(active.join("output.json"), prior).unwrap();
            fs::set_permissions(
                active.join("output.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let held = active.join(format!(".held-recovery-{phase}"));

            let error = manager
                .publish_archive_with(
                    &pinned,
                    &destination,
                    &json!({"text":"new retained output"}),
                    &raw,
                    || Ok(()),
                    (
                        |_pinned, temporary| {
                            if phase == "before-rename" {
                                fs::rename(active.join(temporary), &held).unwrap();
                                fs::write(active.join(temporary), b"replacement owner").unwrap();
                                fs::set_permissions(
                                    active.join(temporary),
                                    fs::Permissions::from_mode(0o600),
                                )
                                .unwrap();
                            }
                            Ok(())
                        },
                        |_pinned, name, temporary| {
                            if phase == "after-rename" {
                                fs::rename(active.join(name), &held).unwrap();
                                fs::write(active.join(name), b"replacement owner").unwrap();
                                fs::set_permissions(
                                    active.join(name),
                                    fs::Permissions::from_mode(0o600),
                                )
                                .unwrap();
                                fs::write(active.join(temporary), b"new temp owner").unwrap();
                                fs::set_permissions(
                                    active.join(temporary),
                                    fs::Permissions::from_mode(0o600),
                                )
                                .unwrap();
                            }
                            Ok(())
                        },
                        rename_directory_noreplace_at,
                        || {},
                        || {},
                        |directory: &File, _label| directory.sync_all(),
                    ),
                )
                .unwrap_err();

            assert!(
                error.to_string().contains("generation changed")
                    || error.to_string().contains("replacement was preserved")
            );
            assert!(active.is_dir());
            assert!(!destination.exists());
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            if phase == "before-rename" {
                assert_eq!(fs::read(active.join("output.json")).unwrap(), prior);
            } else {
                assert_eq!(
                    fs::read(active.join("output.json")).unwrap(),
                    b"replacement owner"
                );
                assert!(held.is_file());
            }
        }
    }

    #[test]
    fn archive_publication_is_noreplace_rolls_back_output_and_reports_fsync_uncertainty() {
        let fixture = Fixture::new();
        let (_token, _digest, _) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let output = fixture.root.join("registry/foreign/output.json");
        let prior = b"{\n  \"prior\": \"exact evidence\"\n}\n";
        fs::write(&output, prior).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let expected_record = manager.record_bytes(&pinned).unwrap();
        std::os::unix::fs::symlink(fixture.root.join("missing"), &destination).unwrap();

        let error = manager
            .publish_archive(
                &pinned,
                &destination,
                &json!({"text":"new"}),
                &expected_record,
                || Ok(()),
            )
            .unwrap_err();
        assert!(error.to_string().contains("existing agent archive"));
        assert_eq!(fs::read(&output).unwrap(), prior);
        assert!(fixture.root.join("registry/foreign").is_dir());

        fs::remove_file(&destination).unwrap();
        let error = manager
            .publish_archive_with(
                &pinned,
                &destination,
                &json!({"text":"new retained output"}),
                &expected_record,
                || Ok(()),
                (
                    |_pinned, _temporary| Ok(()),
                    |_pinned, _name, _temporary| Ok(()),
                    rename_directory_noreplace_at,
                    || {},
                    || {},
                    |_directory: &File, _label| {
                        Err(io::Error::other("injected directory fsync failure"))
                    },
                ),
            )
            .unwrap_err();
        assert!(error.to_string().contains("published"));
        assert!(!fixture.root.join("registry/foreign").exists());
        assert!(destination.is_dir());
        assert_eq!(
            agent::read_private_json(&destination.join("output.json")).unwrap()["text"],
            "new retained output"
        );
    }

    #[test]
    fn publication_reconciles_successful_rename_reported_as_error() {
        for record_read_fails in [false, true] {
            let fixture = Fixture::new();
            let (_token, _digest, raw) = fixture.make_legacy_dead();
            let manager = fixture.manager();
            let record = manager.load("foreign").unwrap();
            let (_archive, destination) = manager.archive_destination(&record).unwrap();
            let active = fixture.root.join("registry/foreign");
            fs::write(active.join("output.json"), b"{\"prior\":true}\n").unwrap();
            fs::set_permissions(
                active.join("output.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let pinned = manager.pinned_agent_directory("foreign").unwrap();

            let error = manager
                .publish_archive_with(
                    &pinned,
                    &destination,
                    &json!({"text":"new retained output"}),
                    &raw,
                    || Ok(()),
                    (
                        |_pinned, _temporary| Ok(()),
                        |_pinned, _name, _temporary| Ok(()),
                        |source_parent, source_name, destination_parent, destination_name| {
                            rename_directory_noreplace_at(
                                source_parent,
                                source_name,
                                destination_parent,
                                destination_name,
                            )?;
                            if record_read_fails {
                                fs::remove_file(destination.join("agent.json")).unwrap();
                                std::os::unix::fs::symlink(
                                    fixture.root.join("missing-agent-record"),
                                    destination.join("agent.json"),
                                )
                                .unwrap();
                            }
                            Err(fail("injected lost successful rename result"))
                        },
                        || {},
                        || {},
                        |directory: &File, _label| directory.sync_all(),
                    ),
                )
                .unwrap_err();

            assert!(error.to_string().contains("reported failure"));
            if record_read_fails {
                assert!(error.to_string().contains("record could not be proved"));
            } else {
                assert!(error.to_string().contains("was published"));
            }
            assert!(!active.exists());
            assert_eq!(
                agent::read_private_json(&destination.join("output.json")).unwrap()["text"],
                "new retained output"
            );
        }
    }

    #[test]
    fn publication_reconciles_late_destination_collision_and_restores_output() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let active = fixture.root.join("registry/foreign");
        let prior = b"{\"prior\":true}\n";
        fs::write(active.join("output.json"), prior).unwrap();
        fs::set_permissions(
            active.join("output.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();

        let error = manager
            .publish_archive(
                &pinned,
                &destination,
                &json!({"text":"new retained output"}),
                &raw,
                || {
                    fs::create_dir(&destination).unwrap();
                    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
                    fs::write(destination.join("sentinel"), b"other owner").unwrap();
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("existing agent archive"));
        assert_eq!(fs::read(active.join("output.json")).unwrap(), prior);
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"other owner"
        );
    }

    #[test]
    fn publication_fsync_failure_attempts_both_and_retains_archive() {
        for failing_label in ["published agent archive", "published agent registry"] {
            let fixture = Fixture::new();
            let (_token, _digest, raw) = fixture.make_legacy_dead();
            let manager = fixture.manager();
            let record = manager.load("foreign").unwrap();
            let (_archive, destination) = manager.archive_destination(&record).unwrap();
            let pinned = manager.pinned_agent_directory("foreign").unwrap();
            let mut calls = Vec::new();

            let error = manager
                .publish_archive_with(
                    &pinned,
                    &destination,
                    &json!({"text":"new retained output"}),
                    &raw,
                    || Ok(()),
                    (
                        |_pinned, _temporary| Ok(()),
                        |_pinned, _name, _temporary| Ok(()),
                        rename_directory_noreplace_at,
                        || {},
                        || {},
                        |_directory, label| {
                            calls.push(label.to_owned());
                            if label == failing_label {
                                Err(io::Error::other("injected publication fsync failure"))
                            } else {
                                Ok(())
                            }
                        },
                    ),
                )
                .unwrap_err();

            assert!(error.to_string().contains("published"));
            assert_eq!(
                calls,
                ["published agent archive", "published agent registry"]
            );
            assert!(!fixture.root.join("registry/foreign").exists());
            assert_eq!(
                agent::read_private_json(&destination.join("output.json")).unwrap()["text"],
                "new retained output"
            );
        }
    }

    #[test]
    fn pinned_publication_refuses_postproof_directory_swap() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let active = fixture.root.join("registry/foreign");
        let displaced = fixture.root.join("registry/.foreign-original");
        let mut published = false;

        fs::rename(&active, &displaced).unwrap();
        fs::create_dir(&active).unwrap();
        fs::set_permissions(&active, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(active.join("agent.json"), &raw).unwrap();
        fs::set_permissions(active.join("agent.json"), fs::Permissions::from_mode(0o600)).unwrap();

        let error = manager
            .publish_pinned_directory_with(
                &pinned,
                &destination,
                &raw,
                &mut published,
                (
                    rename_directory_noreplace_at,
                    || {},
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();
        assert!(!published);

        assert!(error.to_string().contains("registry directory changed"));
        assert!(active.join("agent.json").is_file());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn pinned_publication_refuses_reappeared_source_and_reports_published_state() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let active = fixture.root.join("registry/foreign");

        let error = manager
            .publish_archive_with(
                &pinned,
                &destination,
                &json!({"text":"new retained output"}),
                &raw,
                || Ok(()),
                (
                    |_pinned, _temporary| Ok(()),
                    |_pinned, _name, _temporary| Ok(()),
                    rename_directory_noreplace_at,
                    || {
                        fs::create_dir(&active).unwrap();
                        fs::set_permissions(&active, fs::Permissions::from_mode(0o700)).unwrap();
                        fs::write(active.join("agent.json"), &raw).unwrap();
                        fs::set_permissions(
                            active.join("agent.json"),
                            fs::Permissions::from_mode(0o600),
                        )
                        .unwrap();
                    },
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();

        assert!(error.to_string().contains("rollback was not attempted"));
        assert!(active.is_dir());
        assert!(destination.is_dir());
        assert_eq!(
            agent::read_private_json(&destination.join("output.json")).unwrap()["text"],
            "new retained output"
        );
    }

    #[test]
    fn pinned_publication_refuses_postrename_permission_change() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();

        let error = manager
            .publish_archive_with(
                &pinned,
                &destination,
                &json!({"text":"new retained output"}),
                &raw,
                || Ok(()),
                (
                    |_pinned, _temporary| Ok(()),
                    |_pinned, _name, _temporary| Ok(()),
                    rename_directory_noreplace_at,
                    || {
                        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))
                            .unwrap();
                    },
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();

        assert!(error.to_string().contains("not a private directory"));
        assert!(!fixture.root.join("registry/foreign").exists());
        assert_eq!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            agent::read_private_json(&destination.join("output.json")).unwrap()["text"],
            "new retained output"
        );
    }

    #[test]
    fn pinned_publication_never_promotes_replaced_archive_entry_during_rollback() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let active = fixture.root.join("registry/foreign");
        let displaced = fixture.root.join("registry/archive/.held-original");

        let error = manager
            .publish_archive_with(
                &pinned,
                &destination,
                &json!({"text":"new retained output"}),
                &raw,
                || Ok(()),
                (
                    |_pinned, _temporary| Ok(()),
                    |_pinned, _name, _temporary| Ok(()),
                    rename_directory_noreplace_at,
                    || {
                        fs::rename(&destination, &displaced).unwrap();
                        fs::create_dir(&destination).unwrap();
                        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
                            .unwrap();
                        fs::write(destination.join("attacker-marker"), b"replacement").unwrap();
                    },
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();

        assert!(error.to_string().contains("destination was replaced"));
        assert!(!active.exists());
        assert_eq!(
            fs::read(destination.join("attacker-marker")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            agent::read_private_json(&displaced.join("output.json")).unwrap()["text"],
            "new retained output"
        );
    }

    #[test]
    fn pinned_publication_reproves_generation_after_inverse_rollback() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let active = fixture.root.join("registry/foreign");
        let displaced = fixture.root.join("registry/.foreign-after-rollback");

        let mut wrong_record = raw.clone();
        wrong_record.extend_from_slice(b"mismatch");
        let error = manager
            .publish_archive_with(
                &pinned,
                &destination,
                &json!({"text":"new retained output"}),
                &wrong_record,
                || Ok(()),
                (
                    |_pinned, _temporary| Ok(()),
                    |_pinned, _name, _temporary| Ok(()),
                    rename_directory_noreplace_at,
                    || {},
                    || fs::rename(&active, &displaced).unwrap(),
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("rollback state could not be proved"));
        assert!(!active.exists());
        assert!(!destination.exists());
        assert_eq!(
            agent::read_private_json(&displaced.join("output.json")).unwrap()["text"],
            "new retained output"
        );
    }

    #[test]
    fn pinned_publication_refuses_replaced_archive_parent_and_rolls_back() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let active = fixture.root.join("registry/foreign");
        let displaced_archive = fixture.root.join("registry/.archive-original");
        let mut published = false;

        let error = manager
            .publish_pinned_directory_with(
                &pinned,
                &destination,
                &raw,
                &mut published,
                (
                    rename_directory_noreplace_at,
                    || {
                        fs::rename(&archive, &displaced_archive).unwrap();
                        fs::create_dir(&archive).unwrap();
                        fs::set_permissions(&archive, fs::Permissions::from_mode(0o700)).unwrap();
                    },
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();

        assert!(published);
        assert!(error.to_string().contains("agent archive changed"));
        assert!(active.is_dir());
        assert!(!destination.exists());
        assert!(!displaced_archive
            .join(destination.file_name().unwrap())
            .exists());
    }

    #[test]
    fn pinned_publication_reports_each_rollback_fsync_failure() {
        for failing_label in ["agent archive rollback", "agent registry rollback"] {
            let fixture = Fixture::new();
            let (_token, _digest, raw) = fixture.make_legacy_dead();
            let manager = fixture.manager();
            let record = manager.load("foreign").unwrap();
            let (_archive, destination) = manager.archive_destination(&record).unwrap();
            let pinned = manager.pinned_agent_directory("foreign").unwrap();
            let active_record = fixture.root.join("registry/foreign/agent.json");
            let published_record = destination.join("agent.json");
            let mut calls = Vec::new();

            let error = manager
                .publish_archive_with(
                    &pinned,
                    &destination,
                    &json!({"text":"new retained output"}),
                    &raw,
                    || Ok(()),
                    (
                        |_pinned, _temporary| Ok(()),
                        |_pinned, _name, _temporary| Ok(()),
                        rename_directory_noreplace_at,
                        || {
                            fs::write(&published_record, b"mismatch").unwrap();
                            fs::set_permissions(
                                &published_record,
                                fs::Permissions::from_mode(0o600),
                            )
                            .unwrap();
                        },
                        || {
                            fs::write(&active_record, &raw).unwrap();
                            fs::set_permissions(&active_record, fs::Permissions::from_mode(0o600))
                                .unwrap();
                        },
                        |_directory, label| {
                            calls.push(label.to_owned());
                            if label == failing_label {
                                Err(io::Error::other("injected rollback fsync failure"))
                            } else {
                                Ok(())
                            }
                        },
                    ),
                )
                .unwrap_err();

            let message = error.to_string();
            assert!(message.contains("rollback completed"));
            assert!(message.contains("durability is uncertain"));
            assert!(message.contains(failing_label));
            assert!(fixture.root.join("registry/foreign").is_dir());
            assert!(!destination.exists());
            assert_eq!(calls, ["agent archive rollback", "agent registry rollback"]);
            assert_eq!(
                agent::read_private_json(&fixture.root.join("registry/foreign/output.json"))
                    .unwrap()["text"],
                "new retained output"
            );
        }
    }

    #[test]
    fn pinned_publication_refuses_postproof_record_swap() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let record_path = fixture.root.join("registry/foreign/agent.json");
        let mut published = false;

        let mut document = agent::read_private_json(&record_path).unwrap();
        document["goal"] = json!("replacement record");
        agent::atomic_json(&record_path, &document).unwrap();

        let error = manager
            .publish_pinned_directory_with(
                &pinned,
                &destination,
                &raw,
                &mut published,
                (
                    rename_directory_noreplace_at,
                    || {},
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap_err();
        assert!(published);

        assert!(error
            .to_string()
            .contains("record changed during archival publication"));
        assert!(fixture.root.join("registry/foreign").is_dir());
        assert!(!destination.exists());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn pinned_publication_consumes_original_generation_after_safe_aba() {
        let fixture = Fixture::new();
        let (_token, _digest, raw) = fixture.make_legacy_dead();
        let manager = fixture.manager();
        let record = manager.load("foreign").unwrap();
        let (_archive, destination) = manager.archive_destination(&record).unwrap();
        let pinned = manager.pinned_agent_directory("foreign").unwrap();
        let active = fixture.root.join("registry/foreign");
        let held = fixture.root.join("registry/.foreign-held");
        let mut published = false;

        fs::rename(&active, &held).unwrap();
        fs::create_dir(&active).unwrap();
        fs::set_permissions(&active, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(active.join("agent.json"), &raw).unwrap();
        fs::remove_file(active.join("agent.json")).unwrap();
        fs::remove_dir(&active).unwrap();
        fs::rename(&held, &active).unwrap();
        manager
            .publish_pinned_directory_with(
                &pinned,
                &destination,
                &raw,
                &mut published,
                (
                    rename_directory_noreplace_at,
                    || {},
                    || {},
                    |directory: &File, _label| directory.sync_all(),
                ),
            )
            .unwrap();
        assert!(published);

        assert_eq!(
            fs::symlink_metadata(&destination).unwrap().ino(),
            pinned.inode
        );
        assert!(!active.exists());
    }

    #[test]
    fn explicit_legacy_recovery_preflights_archive_and_snapshot_failures() {
        for case in ["collision", "snapshot"] {
            let fixture = Fixture::new();
            let (token, digest, raw) = fixture.make_legacy_dead();
            let active = fixture.root.join("registry/foreign");
            if case == "collision" {
                let destination = fixture
                    .root
                    .join(format!("registry/archive/foreign-{token}"));
                fs::create_dir_all(&destination).unwrap();
                fs::set_permissions(
                    destination.parent().unwrap(),
                    fs::Permissions::from_mode(0o700),
                )
                .unwrap();
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();
            } else {
                fs::set_permissions(&active, fs::Permissions::from_mode(0o500)).unwrap();
            }
            let result = fixture.manager().stop_with_options(
                "foreign",
                StopOptions {
                    expected_token: Some(token),
                    recover_legacy_adoption: true,
                    retire_dead_adoption: false,
                    expected_record_sha256: Some(digest),
                    skip_cloud_halt: false,
                },
            );
            fs::set_permissions(&active, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(result.is_err(), "{case} unexpectedly succeeded");
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert_eq!(fs::read(active.join("agent.json")).unwrap(), raw);
        }
    }

    #[test]
    fn stopping_live_adopted_agents_refuses_replaced_shell_generations() {
        let original = Fake::foreign_shell_identity();
        let mut replacements = Vec::new();
        let mut new_pane_shell = original.clone();
        new_pane_shell.pid += 1;
        replacements.push(new_pane_shell);
        let mut reused_pid = original.clone();
        reused_pid.starttime_ticks += 1;
        replacements.push(reused_pid);
        let mut replaced_image = original;
        replaced_image.executable_inode += 1;
        replacements.push(replaced_image);

        for replacement in replacements {
            let fixture = Fixture::new();
            fixture.adopt();
            *fixture.client.foreign_shell_identity.lock().unwrap() = replacement;

            let error = fixture.manager().stop("foreign").unwrap_err();

            assert!(error
                .to_string()
                .contains("recorded pane shell generation changed"));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.client.started.load(Ordering::Relaxed));
            assert_eq!(
                fixture.manager().load("foreign").unwrap().lifecycle,
                "running"
            );
        }
    }

    #[test]
    fn stopping_a_sessionless_live_adopted_agent_refuses_replaced_shell_generation() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let adopted = fixture.adopt();
        assert!(adopted["session_value"].is_null());
        fixture
            .client
            .foreign_shell_identity
            .lock()
            .unwrap()
            .starttime_ticks += 1;

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("recorded pane shell generation changed"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(fixture.client.started.load(Ordering::Relaxed));
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_live_legacy_adopted_agent_refuses_missing_shell_identity() {
        let fixture = Fixture::new();
        fixture.adopt();
        let path = fixture.root.join("registry/foreign/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .remove("foreign_shell_identity");
        agent::atomic_json(&path, &document).unwrap();

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("legacy record has no identity-bound"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(fixture.client.started.load(Ordering::Relaxed));
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_live_adopted_agent_refuses_shell_change_during_capture() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture
            .client
            .change_foreign_shell_on_read
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("identity could not be reverified"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(fixture.client.started.load(Ordering::Relaxed));
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_an_absent_adopted_agent_refuses_shell_change_during_capture() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture
            .client
            .change_foreign_shell_on_read
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("identity could not be reverified"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_an_absent_adopted_agent_refuses_cwd_change() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture
            .client
            .wrong_foreign_cwd
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error.to_string().contains("workspace, or cwd changed"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_an_absent_adopted_agent_refuses_a_moved_tab() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture.client.panes.lock().unwrap()[0].tab_id = "moved-tab".to_owned();

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("recorded tab changed while the agent was absent"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_revalidated_live_adopted_agent_allows_a_moved_tab() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.panes.lock().unwrap()[0].tab_id = "moved-tab".to_owned();

        let stopped = fixture.manager().stop("foreign").unwrap();

        assert_eq!(stopped["runtime_preserved"], true);
        assert_eq!(stopped["pane_closed"], false);
        assert_eq!(stopped["tab_closed"], false);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(fixture.client.panes.lock().unwrap()[0].tab_id, "moved-tab");
        assert!(fixture.manager().list().unwrap().is_empty());
    }

    #[test]
    fn stopping_an_adopted_agent_refuses_missing_or_duplicated_recorded_pane() {
        for presentation_count in [0, 2] {
            let fixture = Fixture::new();
            fixture.adopt();
            let original = fixture.client.panes.lock().unwrap()[0].clone();
            let mut panes = fixture.client.panes.lock().unwrap();
            panes.clear();
            if presentation_count == 2 {
                panes.push(original.clone());
                let mut duplicate = original;
                duplicate.tab_id = "duplicate-tab".to_owned();
                panes.push(duplicate);
            }
            drop(panes);

            let error = fixture.manager().stop("foreign").unwrap_err();

            assert!(error.to_string().contains(&format!(
                "expected one recorded pane, found {presentation_count}"
            )));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert_eq!(
                fixture.manager().load("foreign").unwrap().lifecycle,
                "running"
            );
        }
    }

    #[test]
    fn stopping_an_adopted_agent_archives_a_workspace_or_pane_herdr_reports_closed() {
        for (closed, source) in [("workspace", "workspace-missing"), ("pane", "pane-missing")] {
            let fixture = Fixture::new();
            let original = fixture.adopt();
            let record = fixture.manager().load("foreign").unwrap();
            let pane_id = record.pane_id.clone().unwrap();
            fixture.client.panes.lock().unwrap().clear();
            let mut gone = fixture.client.herdr_closed.lock().unwrap();
            gone.insert(pane_id.clone());
            if closed == "workspace" {
                gone.insert(record.workspace_id.clone().unwrap());
            }
            drop(gone);

            let stopped = fixture.manager().stop("foreign").unwrap();

            assert_eq!(stopped["source"], source, "{stopped}");
            assert_eq!(stopped["pane_closed"], false);
            assert_eq!(stopped["tab_closed"], false);
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.manager().load("foreign").is_err());
            let archive = PathBuf::from(stopped["archive"].as_str().unwrap());
            let saved = agent::read_private_json(&archive.join("agent.json")).unwrap();
            assert_eq!(saved["token"], original["token"]);
            assert_eq!(saved["lifecycle"], "stopped");
            let reason = agent::read_private_json(&archive.join("stop.json")).unwrap();
            assert_eq!(reason["source"], source);
            assert_eq!(reason["pane_id"], pane_id.as_str());
            assert!(
                reason["detail"].as_str().unwrap().contains("_not_found"),
                "{reason}"
            );
        }
    }

    #[test]
    fn stopping_an_adopted_agent_missing_from_the_pane_list_refuses_during_an_outage() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.panes.lock().unwrap().clear();
        fixture
            .client
            .fail_pane_info_once
            .store(true, Ordering::SeqCst);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("expected one recorded pane, found 0"),
            "{error}"
        );
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_live_adopted_agent_refuses_a_replaced_native_session() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture
            .client
            .change_session_after_save
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error.to_string().contains("exactly one live pane"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_dead_adopted_agent_refuses_leaving_idle_during_capture() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture
            .client
            .leave_idle_shell_on_read
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("identity could not be reverified"));
        assert!(message.contains("identity-bound idle shell"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_dead_adopted_agent_refuses_restart_during_capture() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture.client.started.store(false, Ordering::Relaxed);
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture
            .client
            .restart_foreign_on_read
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error.to_string().contains("runtime identity changed"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn stopping_a_sessionless_adopted_agent_refuses_a_new_session_during_capture() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture.adopt();
        fixture
            .client
            .restart_foreign_on_read
            .store(true, Ordering::Relaxed);

        let error = fixture.manager().stop("foreign").unwrap_err();

        assert!(error
            .to_string()
            .contains("runtime identity changed during output capture"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(
            fixture.manager().load("foreign").unwrap().lifecycle,
            "running"
        );
    }

    #[test]
    fn adoption_refuses_identity_mismatches_without_creating_a_record() {
        let fixture = Fixture::new();
        fixture
            .client
            .panes
            .lock()
            .unwrap()
            .push(Fake::pane("owned"));
        let manager = fixture.manager();
        assert!(manager.adopt("foreign", fixture.adopt_options()).is_err());
        assert!(!fixture.root.join("registry/foreign").exists());

        let fixture = Fixture::new();
        fixture.prepare_foreign();
        let mut options = fixture.adopt_options();
        options.harness = "claude".to_owned();
        assert!(fixture.manager().adopt("foreign", options).is_err());
        assert!(!fixture.root.join("registry/foreign").exists());

        let fixture = Fixture::new();
        fixture.prepare_foreign();
        let mut options = fixture.adopt_options();
        options.expected_workspace = "wrong".to_owned();
        assert!(fixture.manager().adopt("foreign", options).is_err());
        assert!(!fixture.root.join("registry/foreign").exists());

        let fixture = Fixture::new();
        fixture.prepare_foreign();
        let wrong = fixture.root.join("wrong");
        fs::create_dir(&wrong).unwrap();
        let mut options = fixture.adopt_options();
        options.cwd = wrong;
        assert!(fixture.manager().adopt("foreign", options).is_err());
        assert!(!fixture.root.join("registry/foreign").exists());

        let fixture = Fixture::new();
        fixture.prepare_foreign();
        let mut options = fixture.adopt_options();
        options.session = Some("wrong-thread".to_owned());
        assert!(fixture.manager().adopt("foreign", options).is_err());
        assert!(!fixture.root.join("registry/foreign").exists());
    }

    #[test]
    fn adoption_refuses_a_duplicate_pane_and_preserves_the_first_generation() {
        let fixture = Fixture::new();
        let adopted = fixture.adopt();
        let error = fixture
            .manager()
            .adopt("second", fixture.adopt_options())
            .unwrap_err();
        assert!(error.to_string().contains("already registered as"));
        assert_eq!(
            fixture.manager().get("foreign").unwrap()["token"],
            adopted["token"]
        );
        assert!(!fixture.root.join("registry/second").exists());
    }

    #[test]
    fn same_provider_local_session_id_is_allowed_for_different_harnesses() {
        let fixture = Fixture::new();
        fixture.adopt();
        let mut pane = Fake::pane("claude");
        pane.tab_id = "claude-tab".to_owned();
        fixture.client.panes.lock().unwrap().push(pane);
        let mut options = fixture.adopt_options();
        options.pane_id = "claude".to_owned();
        options.harness = "claude".to_owned();
        let adopted = fixture.manager().adopt("claude", options).unwrap();
        assert_eq!(adopted["session_agent"], "claude");
        assert_eq!(adopted["session_value"], "thread");
        assert_eq!(fixture.manager().list().unwrap().len(), 2);
    }

    #[test]
    fn list_reports_an_unreadable_record_as_its_own_row() {
        let fixture = Fixture::new();
        fixture.adopt();
        let broken = fixture.root.join("registry/broken");
        fs::create_dir(&broken).unwrap();
        fs::set_permissions(&broken, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(broken.join("agent.json"), b"{").unwrap();
        fs::set_permissions(broken.join("agent.json"), fs::Permissions::from_mode(0o600)).unwrap();

        let rows = fixture.manager().list().unwrap();
        assert_eq!(rows.len(), 2);
        let bad = rows.iter().find(|row| row["name"] == "broken").unwrap();
        assert_eq!(bad["record_error"], true);
        assert_eq!(bad["agent_status"], "unknown");
        assert!(!bad["probe_error"].as_str().unwrap().is_empty());
        let good = rows.iter().find(|row| row["name"] == "foreign").unwrap();
        assert!(good.get("record_error").is_none());

        let nested = fixture.root.join("registry/nested");
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            nested.join("agent.json"),
            br#"{"schema":"agentctl-session/v3","name":"nested"}"#,
        )
        .unwrap();
        fs::set_permissions(nested.join("agent.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let rows = fixture.manager().list().unwrap();
        let row = rows.iter().find(|row| row["name"] == "nested").unwrap();
        assert_eq!(row["record_error"], true);
        assert!(row["probe_error"]
            .as_str()
            .unwrap()
            .contains("invalid agent record agentctl-session/v3 fields"));
    }

    #[test]
    fn adoption_refuses_same_harness_session_held_by_headless_record() {
        let fixture = Fixture::new();
        fixture.start(None);
        let path = fixture.root.join("registry/worker/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document["adapter"] = json!("turn-runner");
        document["mode"] = json!("headless");
        document["backend"] = json!("tmux");
        document["runtime_home"] = json!(fixture.root.join("runtime"));
        document["session_agent"] = Value::Null;
        document["session_value"] = json!("thread");
        document["pane_id"] = json!("headless-pane");
        agent::atomic_json(&path, &document).unwrap();
        *fixture.client.panes.lock().unwrap() = vec![Fake::pane("foreign")];
        fixture
            .client
            .duplicate_session
            .store(true, Ordering::Relaxed);
        let mut options = fixture.adopt_options();
        options.pane_id = "foreign".to_owned();
        let error = fixture.manager().adopt("foreign", options).unwrap_err();
        assert!(error.to_string().contains("already registered as"));
        assert!(!fixture.root.join("registry/foreign").exists());
    }

    #[test]
    fn interactive_start_refuses_resume_claimed_by_adopted_agent() {
        let fixture = Fixture::new();
        fixture.adopt();
        let presentation = Fake::pane("owned");
        let error = fixture
            .manager()
            .start(
                "second",
                &fixture.root,
                StartOptions {
                    resume: Some("thread".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("already registered as"));
        assert!(!fixture.root.join("registry/second").exists());
        assert_eq!(*fixture.client.panes.lock().unwrap(), [presentation]);
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn start_refuses_unregistered_same_session_in_another_workspace_and_is_stoppable() {
        let fixture = Fixture::new();
        let mut external = Fake::pane("external");
        external.tab_id = "external-tab".to_owned();
        external.workspace_id = "other-workspace".to_owned();
        fixture.client.panes.lock().unwrap().push(external.clone());
        fixture
            .client
            .duplicate_session
            .store(true, Ordering::Relaxed);
        let error = fixture
            .manager()
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("not globally unique"), "{error}");
        let failed = fixture.manager().load("worker").unwrap();
        assert_eq!(failed.lifecycle, "launch_failed");
        assert!(failed.session_value.is_none());
        assert_eq!(
            fixture.manager().stop("worker").unwrap()["pane_closed"],
            true
        );
        assert_eq!(*fixture.client.panes.lock().unwrap(), [external]);
    }

    #[test]
    fn conflicting_started_pane_remains_stoppable_if_initial_close_fails() {
        let fixture = Fixture::new();
        fixture.start(None);
        let holder_path = fixture.root.join("registry/worker/agent.json");
        let mut holder = agent::read_private_json(&holder_path).unwrap();
        holder["adapter"] = json!("turn-runner");
        holder["mode"] = json!("headless");
        holder["backend"] = json!("tmux");
        holder["runtime_home"] = json!(fixture.root.join("runtime"));
        holder["pane_id"] = json!("headless-pane");
        agent::atomic_json(&holder_path, &holder).unwrap();
        fixture.client.panes.lock().unwrap().clear();
        fixture.client.fail_close.store(true, Ordering::Relaxed);
        let error = fixture
            .manager()
            .start(
                "second",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("could not close"), "{error}");
        let failed = fixture.manager().load("second").unwrap();
        assert_eq!(failed.lifecycle, "launch_failed");
        assert!(failed.session_agent.is_none() && failed.session_value.is_none());
        fixture.client.fail_close.store(false, Ordering::Relaxed);
        assert_eq!(
            fixture.manager().stop("second").unwrap()["pane_closed"],
            true
        );
    }

    #[test]
    fn adoption_refuses_a_reported_session_visible_in_two_live_panes() {
        let fixture = Fixture::new();
        fixture.prepare_foreign();
        let mut duplicate = Fake::pane("duplicate");
        duplicate.tab_id = "other-tab".to_owned();
        fixture.client.panes.lock().unwrap().push(duplicate);
        fixture
            .client
            .duplicate_session
            .store(true, Ordering::Relaxed);
        let error = fixture
            .manager()
            .adopt("foreign", fixture.adopt_options())
            .unwrap_err();
        assert!(error.to_string().contains("exactly one live pane"));
        assert!(!fixture.root.join("registry/foreign").exists());
    }

    #[test]
    fn adoption_archives_failed_generation_if_identity_changes_after_save() {
        for initially_reported in [true, false] {
            let fixture = Fixture::new();
            fixture.prepare_foreign();
            fixture
                .client
                .report_session
                .store(initially_reported, Ordering::Relaxed);
            fixture
                .client
                .change_session_after_save
                .store(true, Ordering::Relaxed);
            let error = fixture
                .manager()
                .adopt("foreign", fixture.adopt_options())
                .unwrap_err();
            assert!(error.to_string().contains("was not registered"));
            assert!(!fixture.root.join("registry/foreign").exists());
            let records: Vec<PathBuf> = fs::read_dir(fixture.root.join("registry/archive"))
                .unwrap()
                .map(|entry| entry.unwrap().path().join("agent.json"))
                .collect();
            assert_eq!(records.len(), 1);
            let saved = agent::read_private_json(&records[0]).unwrap();
            assert_eq!(saved["lifecycle"], "adopt_failed");
            assert!(saved["error"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.client.runs.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn sessionless_adoption_stays_pane_bound_after_goal_session_binding() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let adopted = fixture.adopt();
        let manager = fixture.manager();
        assert!(adopted["session_value"].is_null());
        manager
            .bind_session("foreign", "explicit-thread", None)
            .unwrap();
        manager
            .send("foreign", "still pane bound", DrainOptions::default())
            .unwrap();
        let binding =
            agent::read_private_json(&fixture.root.join("registry/foreign/queue/target.json"))
                .unwrap();
        assert_eq!(binding["kind"], "pane");
        assert_eq!(binding["pane_id"], "owned");
    }

    #[test]
    fn bind_session_refuses_an_authoritative_owner_in_another_record() {
        let fixture = Fixture::new();
        fixture.adopt();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        let mut second = Fake::pane("second");
        second.tab_id = "second-tab".to_owned();
        fixture.client.panes.lock().unwrap().push(second);
        let mut options = fixture.adopt_options();
        options.pane_id = "second".to_owned();
        fixture.manager().adopt("second", options).unwrap();
        let error = fixture
            .manager()
            .bind_session("second", "thread", None)
            .unwrap_err();
        assert!(error.to_string().contains("already registered as"));
        assert!(fixture
            .manager()
            .load("second")
            .unwrap()
            .goal_session_id
            .is_none());
    }

    #[test]
    fn bind_session_refuses_unregistered_live_session_contradiction() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture.adopt();
        let mut reported = Fake::pane("reported");
        reported.tab_id = "reported-tab".to_owned();
        fixture.client.panes.lock().unwrap().push(reported);
        let error = fixture
            .manager()
            .bind_session("foreign", "thread", None)
            .unwrap_err();
        assert!(error.to_string().contains("another or ambiguous live pane"));
        assert!(fixture
            .manager()
            .load("foreign")
            .unwrap()
            .goal_session_id
            .is_none());
    }

    #[test]
    fn concurrent_bind_session_allows_exactly_one_provider_local_owner() {
        let fixture = Fixture::new();
        fixture
            .client
            .report_session
            .store(false, Ordering::Relaxed);
        fixture.adopt();
        let mut second = Fake::pane("second");
        second.tab_id = "second-tab".to_owned();
        fixture.client.panes.lock().unwrap().push(second);
        let mut options = fixture.adopt_options();
        options.pane_id = "second".to_owned();
        let manager = fixture.manager();
        manager.adopt("second", options).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for name in ["foreign", "second"] {
                let barrier = Arc::clone(&barrier);
                let outcomes = Arc::clone(&outcomes);
                let manager = &manager;
                scope.spawn(move || {
                    barrier.wait();
                    let bound = manager.bind_session(name, "shared-thread", None).is_ok();
                    outcomes.lock().unwrap().push(bound);
                });
            }
            barrier.wait();
        });
        let mut outcomes = outcomes.lock().unwrap().clone();
        outcomes.sort_unstable();
        assert_eq!(outcomes, [false, true]);
        let claims = [
            manager.load("foreign").unwrap().goal_session_id,
            manager.load("second").unwrap().goal_session_id,
        ];
        assert_eq!(
            claims
                .iter()
                .filter(|claim| claim.as_deref() == Some("shared-thread"))
                .count(),
            1
        );
    }

    #[test]
    fn failed_probe_after_allocation_preserves_exact_pane_for_cleanup() {
        let fixture = Fixture::new();
        fixture.client.fail_panes.store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        assert!(manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    ..StartOptions::default()
                }
            )
            .is_err());
        let record = manager.get("worker").unwrap();
        assert_eq!(record["pane_id"], "owned");
        assert_eq!(record["tab_id"], "tab");
        assert_eq!(record["lifecycle"], "launch_failed");
        fixture.client.fail_panes.store(false, Ordering::Relaxed);
        assert_eq!(manager.stop("worker").unwrap()["pane_closed"], true);
    }

    #[test]
    fn failed_muse_launch_with_our_stale_pane_report_is_stoppable() {
        let fixture = Fixture::new();
        fixture
            .client
            .custom_dies_after_report
            .store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        let error = manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("foreground process"));
        let record = manager.load("worker").unwrap();
        assert_eq!(record.lifecycle, "launch_failed");
        assert!(record.pane_reported_by_agentctl);
        assert_eq!(manager.stop("worker").unwrap()["pane_closed"], true);
    }

    #[test]
    fn muse_identity_is_persisted_before_a_trust_failure_and_allows_stop() {
        let fixture = Fixture::new();
        fixture
            .client
            .custom_fails_after_identity
            .store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        let error = manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("trust prompt"));
        let record = manager.load("worker").unwrap();
        assert_eq!(record.lifecycle, "launch_failed");
        assert_eq!(
            record.custom_process_identity,
            Some(Fake::custom_identity())
        );
        assert!(!record.pane_reported_by_agentctl);
        assert_eq!(manager.stop("worker").unwrap()["pane_closed"], true);
    }

    #[test]
    fn persisted_muse_identity_makes_a_starting_record_stoppable() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        let mut record = manager.load("worker").unwrap();
        assert!(record.custom_process_identity.is_some());
        record.lifecycle = "starting".to_owned();
        manager.save(&record).unwrap();
        assert_eq!(manager.stop("worker").unwrap()["pane_closed"], true);
    }

    #[test]
    fn starting_muse_without_report_or_identity_is_not_stoppable() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        let mut record = manager.load("worker").unwrap();
        record.lifecycle = "starting".to_owned();
        record.pane_reported_by_agentctl = false;
        record.custom_process_identity = None;
        manager.save(&record).unwrap();
        let error = manager.stop("worker").unwrap_err();
        assert!(error.to_string().contains("cannot prove starting"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn running_muse_record_does_not_gain_idle_shell_cleanup_fallback() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        fixture.client.custom_alive.store(false, Ordering::Relaxed);
        let error = manager.stop("worker").unwrap_err();
        assert!(error.to_string().contains("foreground process"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn custom_process_identity_record_is_atomic_and_strict() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .unwrap();
        let path = fixture.root.join("registry/worker/agent.json");
        let mut record = agent::read_private_json(&path).unwrap();
        record["custom_process_identity"]
            .as_object_mut()
            .unwrap()
            .remove("starttime_ticks");
        agent::atomic_json(&path, &record).unwrap();
        let error = manager.load("worker").unwrap_err();
        assert!(error.to_string().contains("invalid agent record"));
    }

    #[test]
    fn foreign_shell_identity_record_is_strict_but_legacy_absence_loads() {
        let fixture = Fixture::new();
        fixture.adopt();
        let manager = fixture.manager();
        let path = fixture.root.join("registry/foreign/agent.json");
        let original = agent::read_private_json(&path).unwrap();

        let mut malformed = original.clone();
        malformed["foreign_shell_identity"]
            .as_object_mut()
            .unwrap()
            .remove("starttime_ticks");
        agent::atomic_json(&path, &malformed).unwrap();
        assert!(manager
            .load("foreign")
            .unwrap_err()
            .to_string()
            .contains("invalid agent record"));

        let mut wrong_adapter = original.clone();
        wrong_adapter["adapter"] = json!("herdr");
        agent::atomic_json(&path, &wrong_adapter).unwrap();
        assert!(manager
            .load("foreign")
            .unwrap_err()
            .to_string()
            .contains("invalid agent record"));

        let mut legacy = original;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("foreign_shell_identity");
        agent::atomic_json(&path, &legacy).unwrap();
        assert!(manager
            .load("foreign")
            .unwrap()
            .foreign_shell_identity
            .is_none());
    }

    #[test]
    fn failed_muse_launch_cleanup_refuses_a_replacement_report() {
        let fixture = Fixture::new();
        fixture
            .client
            .custom_dies_after_report
            .store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        assert!(manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .is_err());
        fixture
            .client
            .custom_reported
            .store(false, Ordering::Relaxed);
        fixture.client.launches.lock().unwrap().remove("owned");
        let error = manager.stop("worker").unwrap_err();
        assert!(error.to_string().contains("foreground process"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_muse_launch_cleanup_refuses_non_shell_foreground_process() {
        let fixture = Fixture::new();
        fixture
            .client
            .custom_dies_after_report
            .store(true, Ordering::Relaxed);
        let manager = fixture.manager();
        assert!(manager
            .start(
                "worker",
                &fixture.root,
                StartOptions {
                    workspace_id: Some("workspace".to_owned()),
                    harness: "muse".to_owned(),
                    ..StartOptions::default()
                },
            )
            .is_err());
        fixture
            .client
            .custom_at_idle_shell
            .store(false, Ordering::Relaxed);
        let error = manager.stop("worker").unwrap_err();
        assert!(error.to_string().contains("foreground process"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    #[test]
    fn an_old_partial_allocation_can_retire_only_its_unclaimed_shell() {
        let fixture = Fixture::new();
        fixture.start(None);
        let manager = fixture.manager();
        let mut record = manager.load("worker").unwrap();
        record.pane_id = None;
        record.lifecycle = "launch_failed".to_owned();
        manager.save(&record).unwrap();
        fixture.client.started.store(false, Ordering::Relaxed);
        let result = manager.stop("worker").unwrap();
        assert_eq!(result["pane_closed"], true);
        assert_eq!(*fixture.client.closed.lock().unwrap(), ["owned"]);
    }

    #[test]
    fn stop_preserves_a_human_pane_created_after_the_membership_check() {
        let fixture = Fixture::new();
        fixture.start(None);
        fixture
            .client
            .add_sibling_on_read
            .store(true, Ordering::Relaxed);
        let result = fixture.manager().stop("worker").unwrap();
        assert_eq!(result["pane_closed"], true);
        assert_eq!(result["tab_closed"], false);
        assert_eq!(*fixture.client.panes.lock().unwrap(), [Fake::pane("human")]);
        assert_eq!(*fixture.client.closed.lock().unwrap(), ["owned"]);
    }

    #[test]
    fn managed_dead_stop_requires_token_then_closes_exact_pane_and_archives() {
        let fixture = Fixture::new();
        let (pane, token) = fixture.make_managed_dead();
        let error = fixture.manager().stop("worker").unwrap_err();
        assert!(error.to_string().contains("requires --expected-token"));

        let stopped = fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(token),
                    ..StopOptions::default()
                },
            )
            .unwrap();

        assert_eq!(stopped["managed_dead"], true);
        assert_eq!(stopped["pane_closed"], true);
        assert_eq!(stopped["tab_closed"], true);
        assert_eq!(*fixture.client.closed.lock().unwrap(), [pane]);
        let archive = PathBuf::from(stopped["archive"].as_str().unwrap());
        assert_eq!(
            agent::read_private_json(&archive.join("agent.json")).unwrap()["lifecycle"],
            "stopped"
        );
        assert!(archive.join("output.json").is_file());
    }

    #[test]
    fn managed_dead_stop_refuses_expanded_stopped_record_before_close() {
        let fixture = Fixture::new();
        let (_pane, token) = fixture.make_managed_dead();
        let path = fixture.root.join("registry/worker/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document["future_padding"] = json!(vec!["x"; 150_000]);
        let mut raw = serde_json::to_vec(&document).unwrap();
        raw.push(b'\n');
        let mut stopped = document.clone();
        stopped["lifecycle"] = json!("stopped");
        let mut stopped_bytes = serde_json::to_vec_pretty(&stopped).unwrap();
        stopped_bytes.push(b'\n');
        assert!(raw.len() < MAX_AGENT_RECORD_BYTES);
        assert!(stopped_bytes.len() > MAX_AGENT_RECORD_BYTES);
        fs::write(&path, &raw).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(token.clone()),
                    ..StopOptions::default()
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("stopped agent record larger"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(fs::read(&path).unwrap(), raw);
        assert!(!fixture.root.join("registry/worker/output.json").exists());
        assert!(!fixture
            .root
            .join(format!("registry/archive/worker-{token}"))
            .exists());
    }

    #[test]
    fn managed_dead_preparation_never_restores_over_replaced_artifact_generations() {
        for phase in ["before-rename", "after-rename"] {
            let fixture = Fixture::new();
            let (_pane, token) = fixture.make_managed_dead();
            let manager = fixture.manager();
            let record = manager.load("worker").unwrap();
            let pinned = manager.pinned_agent_directory("worker").unwrap();
            let active = fixture.root.join("registry/worker");
            let prior = b"{\"prior\":true}\n";
            fs::write(active.join("output.json"), prior).unwrap();
            fs::set_permissions(
                active.join("output.json"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let held = active.join(format!(".held-managed-{phase}"));

            let error = manager
                .retire_managed_dead_locked_with(
                    &record,
                    &pinned,
                    Some(&token),
                    || {},
                    || {},
                    (
                        |_pinned, temporary| {
                            if phase == "before-rename" {
                                fs::rename(active.join(temporary), &held).unwrap();
                                fs::write(active.join(temporary), b"replacement owner").unwrap();
                                fs::set_permissions(
                                    active.join(temporary),
                                    fs::Permissions::from_mode(0o600),
                                )
                                .unwrap();
                            }
                            Ok(())
                        },
                        |_pinned, name, temporary| {
                            if phase == "after-rename" {
                                fs::rename(active.join(name), &held).unwrap();
                                fs::write(active.join(name), b"replacement owner").unwrap();
                                fs::set_permissions(
                                    active.join(name),
                                    fs::Permissions::from_mode(0o600),
                                )
                                .unwrap();
                                fs::write(active.join(temporary), b"new temp owner").unwrap();
                                fs::set_permissions(
                                    active.join(temporary),
                                    fs::Permissions::from_mode(0o600),
                                )
                                .unwrap();
                            }
                            Ok(())
                        },
                    ),
                )
                .unwrap_err();

            assert!(
                error.to_string().contains("generation changed")
                    || error.to_string().contains("replacement was preserved")
            );
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(active.is_dir());
            assert!(fs::read_dir(fixture.root.join("registry/archive"))
                .unwrap()
                .next()
                .is_none());
            if phase == "before-rename" {
                assert_eq!(fs::read(active.join("output.json")).unwrap(), prior);
            } else {
                assert_eq!(
                    fs::read(active.join("output.json")).unwrap(),
                    b"replacement owner"
                );
                assert!(held.is_file());
            }
        }
    }

    #[test]
    fn managed_dead_stop_refuses_stopping_record_without_prior_proof() {
        let fixture = Fixture::new();
        let (_pane, token) = fixture.make_managed_dead();
        let path = fixture.root.join("registry/worker/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document["lifecycle"] = json!("stopping");
        agent::atomic_json(&path, &document).unwrap();

        let error = fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(token),
                    ..StopOptions::default()
                },
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("requires a running herdr record"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(path.is_file());
    }

    #[test]
    fn managed_dead_stop_refuses_crossed_mode_or_backend() {
        for (field, value) in [("mode", "headless"), ("backend", "tmux")] {
            let fixture = Fixture::new();
            let (_pane, token) = fixture.make_managed_dead();
            let path = fixture.root.join("registry/worker/agent.json");
            let mut document = agent::read_private_json(&path).unwrap();
            document[field] = json!(value);
            agent::atomic_json(&path, &document).unwrap();

            assert!(fixture
                .manager()
                .stop_with_options(
                    "worker",
                    StopOptions {
                        expected_token: Some(token),
                        ..StopOptions::default()
                    },
                )
                .is_err());
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(path.is_file());
        }
    }

    #[test]
    fn managed_dead_stop_refuses_runtime_or_registry_changes_without_closing() {
        for case in [
            "replacement",
            "session",
            "moved",
            "sibling",
            "shell",
            "record",
            "directory",
            "capture",
        ] {
            let fixture = Fixture::new();
            let (_pane, token) = fixture.make_managed_dead();
            match case {
                "replacement" => fixture
                    .client
                    .restart_foreign_on_read
                    .store(true, Ordering::Relaxed),
                "session" => fixture.client.report_session.store(true, Ordering::Relaxed),
                "moved" => fixture.client.panes.lock().unwrap()[0].tab_id = "moved".to_owned(),
                "sibling" => fixture
                    .client
                    .panes
                    .lock()
                    .unwrap()
                    .push(Fake::pane("human")),
                "shell" => fixture
                    .client
                    .change_foreign_shell_on_read
                    .store(true, Ordering::Relaxed),
                "record" => fixture
                    .client
                    .change_record_token_on_read
                    .store(true, Ordering::Relaxed),
                "directory" => fixture
                    .client
                    .replace_record_directory_on_read
                    .store(true, Ordering::Relaxed),
                "capture" => fixture.client.fail_read.store(true, Ordering::Relaxed),
                _ => unreachable!(),
            }
            assert!(fixture
                .manager()
                .stop_with_options(
                    "worker",
                    StopOptions {
                        expected_token: Some(token),
                        ..StopOptions::default()
                    },
                )
                .is_err());
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.root.join("registry/worker").is_dir());
        }
    }

    #[test]
    fn managed_dead_stop_reproves_runtime_after_artifact_preparation() {
        let fixture = Fixture::new();
        let (_pane, token) = fixture.make_managed_dead();
        let manager = fixture.manager();
        let record = manager.load("worker").unwrap();
        let pinned = manager.pinned_agent_directory("worker").unwrap();
        let record_path = fixture.root.join("registry/worker/agent.json");
        let original_record = fs::read(&record_path).unwrap();

        let error = manager
            .retire_managed_dead_locked_with(
                &record,
                &pinned,
                Some(&token),
                || {
                    fixture
                        .client
                        .foreign_shell_identity
                        .lock()
                        .unwrap()
                        .starttime_ticks += 1;
                },
                || {},
                (
                    |_pinned, _temporary| Ok(()),
                    |_pinned, _name, _temporary| Ok(()),
                ),
            )
            .unwrap_err();

        assert!(error.to_string().contains("immediately before close"));
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert_eq!(fs::read(record_path).unwrap(), original_record);
        assert!(!fixture.root.join("registry/worker/output.json").exists());
        assert!(fs::read_dir(fixture.root.join("registry/archive"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn managed_dead_stop_rechecks_registry_after_artifact_preparation() {
        for case in ["record", "directory", "output"] {
            let fixture = Fixture::new();
            let (_pane, token) = fixture.make_managed_dead();
            let manager = fixture.manager();
            let record = manager.load("worker").unwrap();
            let pinned = manager.pinned_agent_directory("worker").unwrap();
            let active = fixture.root.join("registry/worker");
            let record_path = active.join("agent.json");
            let original_record = fs::read(&record_path).unwrap();

            let error = manager
                .retire_managed_dead_locked_with(
                    &record,
                    &pinned,
                    Some(&token),
                    || {
                        if case == "record" {
                            let mut document = agent::read_private_json(&record_path).unwrap();
                            document["goal"] = json!("replacement record");
                            agent::atomic_json(&record_path, &document).unwrap();
                        } else if case == "directory" {
                            let displaced = fixture.root.join("registry/.worker-original");
                            fs::rename(&active, &displaced).unwrap();
                            fs::create_dir(&active).unwrap();
                            fs::set_permissions(&active, fs::Permissions::from_mode(0o700))
                                .unwrap();
                            fs::copy(displaced.join("agent.json"), active.join("agent.json"))
                                .unwrap();
                            fs::set_permissions(
                                active.join("agent.json"),
                                fs::Permissions::from_mode(0o600),
                            )
                            .unwrap();
                        } else {
                            let output = active.join("output.json");
                            fs::rename(&output, active.join(".prepared-output")).unwrap();
                            fs::write(&output, b"replacement output").unwrap();
                            fs::set_permissions(&output, fs::Permissions::from_mode(0o600))
                                .unwrap();
                        }
                    },
                    || {},
                    (
                        |_pinned, _temporary| Ok(()),
                        |_pinned, _name, _temporary| Ok(()),
                    ),
                )
                .unwrap_err();

            assert!(error.to_string().contains("changed"));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fs::read_dir(fixture.root.join("registry/archive"))
                .unwrap()
                .next()
                .is_none());
            if case == "record" {
                assert_eq!(
                    agent::read_private_json(&record_path).unwrap()["goal"],
                    "replacement record"
                );
                assert_ne!(fs::read(record_path).unwrap(), original_record);
                assert!(!active.join("output.json").exists());
            } else if case == "directory" {
                let displaced = fixture.root.join("registry/.worker-original");
                assert_eq!(
                    fs::read(displaced.join("agent.json")).unwrap(),
                    original_record
                );
                assert!(!displaced.join("output.json").exists());
                assert!(active.is_dir());
            } else {
                assert_eq!(fs::read(&record_path).unwrap(), original_record);
                assert_eq!(
                    fs::read(active.join("output.json")).unwrap(),
                    b"replacement output"
                );
            }
        }
    }

    #[test]
    fn managed_dead_stop_rechecks_pane_after_final_record_proof() {
        for case in ["sibling", "agent"] {
            let fixture = Fixture::new();
            let (_pane, token) = fixture.make_managed_dead();
            let manager = fixture.manager();
            let record = manager.load("worker").unwrap();
            let pinned = manager.pinned_agent_directory("worker").unwrap();
            let active = fixture.root.join("registry/worker");

            let error = manager
                .retire_managed_dead_locked_with(
                    &record,
                    &pinned,
                    Some(&token),
                    || {},
                    || {
                        if case == "sibling" {
                            fixture
                                .client
                                .panes
                                .lock()
                                .unwrap()
                                .push(Fake::pane("human"));
                        } else {
                            fixture.client.started.store(true, Ordering::Relaxed);
                        }
                    },
                    (
                        |_pinned, _temporary| Ok(()),
                        |_pinned, _name, _temporary| Ok(()),
                    ),
                )
                .unwrap_err();

            let expected = if case == "sibling" {
                "recorded tab is not the exact one-pane tab"
            } else {
                "pane still reports agent"
            };
            assert!(error.to_string().contains(expected));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(active.is_dir());
            assert!(!active.join("output.json").exists());
            assert!(fs::read_dir(fixture.root.join("registry/archive"))
                .unwrap()
                .next()
                .is_none());
        }
    }

    #[test]
    fn managed_dead_stop_rechecks_pane_after_final_shell_proof() {
        for mutation in [1, 2] {
            let fixture = Fixture::new();
            let (_pane, token) = fixture.make_managed_dead();
            fixture
                .client
                .shell_proof_mutation
                .store(mutation, Ordering::Relaxed);

            let error = fixture
                .manager()
                .stop_with_options(
                    "worker",
                    StopOptions {
                        expected_token: Some(token),
                        ..StopOptions::default()
                    },
                )
                .unwrap_err();

            let expected = if mutation == 1 {
                "recorded tab is not the exact one-pane tab"
            } else {
                "pane still reports agent"
            };
            assert!(error.to_string().contains(expected));
            assert!(fixture.client.closed.lock().unwrap().is_empty());
            assert!(fixture.root.join("registry/worker").is_dir());
            assert!(!fixture.root.join("registry/worker/output.json").exists());
            assert!(fs::read_dir(fixture.root.join("registry/archive"))
                .unwrap()
                .next()
                .is_none());
        }
    }

    #[test]
    fn managed_dead_stop_retries_after_close_result_is_uncertain() {
        let fixture = Fixture::new();
        let (_pane, token) = fixture.make_managed_dead();
        fixture
            .client
            .fail_after_close
            .store(true, Ordering::Relaxed);

        let error = fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(token.clone()),
                    ..StopOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("result loss after close"));
        assert_eq!(
            agent::read_private_json(&fixture.root.join("registry/worker/agent.json")).unwrap()
                ["lifecycle"],
            "running"
        );
        assert!(fixture.client.panes.lock().unwrap().is_empty());

        let stopped = fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(token),
                    ..StopOptions::default()
                },
            )
            .unwrap();
        let archive = PathBuf::from(stopped["archive"].as_str().unwrap());
        assert!(!fixture.root.join("registry/worker").exists());
        assert_eq!(
            agent::read_private_json(&archive.join("agent.json")).unwrap()["lifecycle"],
            "stopped"
        );
    }

    #[test]
    fn managed_dead_stop_preflights_archive_collision_before_close() {
        let fixture = Fixture::new();
        let (_pane, token) = fixture.make_managed_dead();
        let destination = fixture
            .root
            .join(format!("registry/archive/worker-{token}"));
        fs::create_dir_all(&destination).unwrap();
        fs::set_permissions(
            destination.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(fixture
            .manager()
            .stop_with_options(
                "worker",
                StopOptions {
                    expected_token: Some(token),
                    ..StopOptions::default()
                },
            )
            .is_err());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
        assert!(fixture.root.join("registry/worker").is_dir());
    }

    #[test]
    fn paused_input_is_refused_but_human_attach_and_read_remain_available() {
        let fixture = Fixture::new();
        fixture.start(None);
        let manager = fixture.manager();
        let path = fixture.root.join("registry/worker/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document["future_metadata"] = json!({"nested":true});
        agent::atomic_json(&path, &document).unwrap();
        assert_eq!(manager.pause("worker", true).unwrap()["paused"], true);
        assert!(manager
            .send("worker", "task", DrainOptions::default())
            .is_err());
        assert!(manager.drain("worker", DrainOptions::default()).is_err());
        assert!(manager
            .goal("worker", Some("objective"), DrainOptions::default(), None)
            .is_err());
        assert_eq!(manager.attach("worker").unwrap()["paused"], true);
        assert_eq!(manager.read("worker", 10).unwrap(), "visible output");
        assert_eq!(
            manager.get("worker").unwrap()["future_metadata"],
            json!({"nested":true})
        );
        manager.pause("worker", false).unwrap();
        manager
            .send_identified("worker", "task", DrainOptions::default(), Some("request-1"))
            .unwrap();
        assert!(manager
            .send_identified("worker", "task", DrainOptions::default(), Some("request-1"))
            .is_err());
        assert_eq!(*fixture.client.runs.lock().unwrap(), ["task"]);
    }

    #[test]
    fn capture_reads_take_only_the_screen_of_a_pane_that_keeps_no_scrollback() {
        let fixture = Fixture::new();
        fixture.start(None);
        let manager = fixture.manager();
        let runtime = agent::SystemRuntime::default();
        let scroll = |max_offset_from_bottom| crate::client::PaneScroll {
            offset_from_bottom: 0,
            max_offset_from_bottom,
            viewport_rows: 52,
        };
        // Each read persists what it read as the agent's latest snapshot.
        let snapshot = fixture.root.join("registry/worker/output.json");
        let took_snapshot = || {
            let written = agent::read_private_json(&snapshot).unwrap();
            fs::remove_file(&snapshot).unwrap();
            written["text"] == "visible output"
        };
        let _ = fs::remove_file(&snapshot);
        // The service reads the sources `chat tick` reads, and falls back the same way when a
        // `recent-unwrapped` read returns nothing.
        for (reported, unwrapped_empty, sources) in [
            (Some(scroll(0)), false, &["visible"][..]),
            (Some(scroll(0)), true, &["visible"][..]),
            (Some(scroll(120)), false, &["recent-unwrapped"][..]),
            (Some(scroll(120)), true, &["recent-unwrapped", "recent"][..]),
            (None, false, &["recent-unwrapped"][..]),
            (None, true, &["recent-unwrapped", "recent"][..]),
        ] {
            *fixture.client.scroll.lock().unwrap() = reported;
            fixture
                .client
                .unwrapped_empty
                .store(unwrapped_empty, Ordering::Relaxed);
            fixture.client.read_sources.lock().unwrap().clear();
            assert_eq!(
                manager
                    .read_capture_with_runtime("worker", 10, &runtime)
                    .unwrap(),
                "visible output"
            );
            assert!(took_snapshot());
            assert_eq!(
                *fixture.client.read_sources.lock().unwrap(),
                sources,
                "{reported:?} {unwrapped_empty}"
            );
            // The read the service repeats reads the same way and persists nothing.
            fixture.client.read_sources.lock().unwrap().clear();
            assert_eq!(
                manager
                    .peek_capture_with_runtime("worker", 10, &runtime)
                    .unwrap(),
                "visible output"
            );
            assert!(!snapshot.exists());
            assert_eq!(
                *fixture.client.read_sources.lock().unwrap(),
                sources,
                "{reported:?} {unwrapped_empty}"
            );
            fixture.client.read_sources.lock().unwrap().clear();
            assert_eq!(
                manager.read_capture("worker", 10).unwrap(),
                "visible output"
            );
            assert!(took_snapshot());
            assert_eq!(
                *fixture.client.read_sources.lock().unwrap(),
                sources,
                "{reported:?} {unwrapped_empty}"
            );
            // The `agentctl read` command keeps reading recent rows whatever the pane keeps.
            fixture.client.read_sources.lock().unwrap().clear();
            assert_eq!(manager.read("worker", 10).unwrap(), "visible output");
            assert!(took_snapshot());
            assert_eq!(
                *fixture.client.read_sources.lock().unwrap(),
                if unwrapped_empty {
                    &["recent-unwrapped", "recent"][..]
                } else {
                    &["recent-unwrapped"][..]
                }
            );
        }
    }

    #[test]
    fn foreign_runtime_metadata_is_visible_without_herdr_mutation() {
        let fixture = Fixture::new();
        fixture.start(None);
        let path = fixture.root.join("registry/worker/agent.json");
        let mut document = agent::read_private_json(&path).unwrap();
        document["adapter"] = json!("turn-runner");
        document["mode"] = json!("headless");
        document["runtime_home"] = json!("/tmp/worker-runtime");
        agent::atomic_json(&path, &document).unwrap();
        let manager = fixture.manager();
        let status = manager.status("worker").unwrap();
        assert_eq!(status["adapter"], "turn-runner");
        assert_eq!(status["agent_status"], "unknown");
        assert!(status["probe_error"]
            .as_str()
            .unwrap()
            .contains("is a headless turn-runner worker"));
        assert_eq!(manager.list().unwrap().len(), 1);
        assert!(manager.stop("worker").is_err());
        assert!(fixture.client.closed.lock().unwrap().is_empty());
    }

    /// Offline retirement of an exact adopted generation with no runtime mutation.
    mod dead_adoption;
    /// Herdr readiness and adopted Muse process ownership compatibility.
    mod herdr_compatibility;
    /// Recipient checks around input, anchors, rename transactions and doctor.
    mod identity;
    /// Muse 1.4 paused, idle, staged and working status, and the input each one admits.
    mod muse_status;
    /// Nested agentctl-session/v2 and v3 rows: decode, write-back and anchor migration.
    mod nested;
    /// Recorded conversation and launch policy, with no status-time mutation.
    mod recovery_metadata;
    /// Resume recovery, immutable planning, exact publication and crash reconciliation.
    mod revive;
    /// Trusted stop refusals and generation-bound recovery advice.
    mod stop_advice;
    /// Retirement of headless turn-runner records whose runner is provably dead.
    mod turn_runner;
}
