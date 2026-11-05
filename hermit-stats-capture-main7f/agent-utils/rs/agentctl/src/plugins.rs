//! Manifest-only discovery for user-installed process plugins.
//!
//! Discovery never executes a file. The user-level home defaults to `~/.agentctl` and can be
//! replaced with `AGENTCTL_HOME` for an isolated installation or test. Each direct child of
//! `plugins/` must be a private, current-user-owned directory containing `manifest.json` and the
//! one direct-child executable named by that manifest. Symlinks, path traversal, hard-linked
//! files, unsafe modes, oversized manifests, duplicate names/capabilities, and incompatible
//! protocols are reported as refusals.
//!
//! Runtime truth is deliberately absent from manifests. Static discovery can establish that an
//! implementation is installed, but it cannot establish configuration, a connection, or a live
//! verification. Those fields therefore remain false until a future runtime probe produces its
//! own evidence.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use chat_subscription_plugin::process::{
    ProcessPhaseTimeouts, ProcessPluginBackend, ProcessPluginCancellation,
    ProcessPluginChild as SupervisedPluginChild, ProcessPluginError,
};

/// Environment override for the user-level plugin and control home.
pub const AGENTCTL_HOME_ENV: &str = "AGENTCTL_HOME";
/// Maximum number of direct plugin entries inspected in one invocation.
pub const MAX_PLUGIN_ENTRIES: usize = 128;
/// Maximum encoded manifest size.
pub const MAX_MANIFEST_BYTES: u64 = 65_536;
/// Current manifest schema identity.
pub const MANIFEST_SCHEMA: &str = "agentctl-plugin-manifest/v1alpha1";
/// Maximum operator-selected environment names forwarded to one plugin.
pub const MAX_PLUGIN_ENVIRONMENT_NAMES: usize = 64;
/// Maximum UTF-8 bytes in one operator-selected environment name.
pub const MAX_PLUGIN_ENVIRONMENT_NAME_BYTES: usize = 128;
/// Compatibility Hello deadline used when a manifest does not declare process phase timeouts.
pub(crate) const DEFAULT_PLUGIN_HELLO_TIMEOUT_SECONDS: u64 = 10;
/// Compatibility Start deadline used when a manifest does not declare process phase timeouts.
pub(crate) const DEFAULT_PLUGIN_START_TIMEOUT_SECONDS: u64 = 30;
/// Compatibility Commit deadline used when a manifest does not declare process phase timeouts.
pub(crate) const DEFAULT_PLUGIN_COMMIT_TIMEOUT_SECONDS: u64 = 30;
/// Compatibility Close deadline used when a manifest does not declare process phase timeouts.
pub(crate) const DEFAULT_PLUGIN_CLOSE_TIMEOUT_SECONDS: u64 = 10;
/// Compatibility cooperative-exit grace used when a manifest omits process phase timeouts.
pub(crate) const DEFAULT_PLUGIN_SHUTDOWN_GRACE_SECONDS: u64 = 2;
/// Largest Hello deadline a plugin manifest may request.
pub(crate) const MAX_PLUGIN_HELLO_TIMEOUT_SECONDS: u64 = 30;
/// Largest Start deadline a plugin manifest may request.
pub(crate) const MAX_PLUGIN_START_TIMEOUT_SECONDS: u64 = 180;
/// Largest Commit deadline a plugin manifest may request.
pub(crate) const MAX_PLUGIN_COMMIT_TIMEOUT_SECONDS: u64 = 60;
/// Largest Close deadline a plugin manifest may request.
pub(crate) const MAX_PLUGIN_CLOSE_TIMEOUT_SECONDS: u64 = 120;
/// Largest cooperative-exit grace a plugin manifest may request.
pub(crate) const MAX_PLUGIN_SHUTDOWN_GRACE_SECONDS: u64 = 10;
/// Non-secret process context explicitly retained after `env_clear`.
pub const PLUGIN_BASELINE_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    owner: u32,
    links: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
            owner: metadata.uid(),
            links: metadata.nlink(),
        }
    }
}

/// Separately reported implementation and runtime maturity facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MaturityStatus {
    /// Code for this backend exists according to trusted built-in knowledge or its manifest.
    pub implemented: bool,
    /// Required operator configuration has been validated by a runtime probe.
    pub configured: bool,
    /// A current provider connection has been established.
    pub connected: bool,
    /// A real end-to-end event and commit was verified during this connection.
    pub live_verified: bool,
}

impl MaturityStatus {
    /// Describe statically discovered code without claiming runtime evidence.
    #[must_use]
    pub fn discovered(implemented: bool) -> Self {
        Self {
            implemented,
            configured: false,
            connected: false,
            live_verified: false,
        }
    }
}

/// One safe, compatible manifest discovered without executing its program.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveredPlugin {
    /// Manifest name, equal to its containing directory.
    pub name: String,
    /// Globally unique capability identity.
    pub capability: String,
    /// Relative direct-child executable filename.
    pub executable: String,
    /// Stable framed protocol family.
    pub protocol_name: &'static str,
    /// Lowest framed protocol version accepted by the plugin.
    pub protocol_min: u16,
    /// Highest framed protocol version accepted by the plugin.
    pub protocol_max: u16,
    /// Explicitly distinguishes user discovery from built-in code.
    pub origin: &'static str,
    /// Facts static discovery can establish; runtime facts remain false.
    pub status: MaturityStatus,
    /// Validated process-protocol phase policy; omitted from capability JSON.
    #[serde(skip)]
    pub(crate) process_phase_timeouts: ProcessPhaseTimeouts,
    /// Exact executable accepted during discovery; omitted from capability JSON.
    #[serde(skip)]
    pub(crate) executable_identity: Option<FileIdentity>,
}

/// One directory entry refused before execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RefusedPlugin {
    /// Bounded display name for the offending entry.
    pub entry: String,
    /// Stable machine-readable refusal class.
    pub code: &'static str,
    /// Bounded public diagnostic.
    pub detail: String,
}

/// Complete plugin-home discovery result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PluginInventory {
    /// Resolved user-level home, absent only when neither environment source was usable.
    pub home: Option<PathBuf>,
    /// Safe compatible manifests. Their executables have not been run.
    pub discovered: Vec<DiscoveredPlugin>,
    /// Unsafe, malformed, duplicate, or incompatible entries.
    pub refused: Vec<RefusedPlugin>,
}

impl PluginInventory {
    /// Pin and revalidate a command with a cleared environment and minimal non-secret baseline.
    ///
    /// The manifest cannot request environment variables or arguments. This compatibility method
    /// selects no extra names; hosts with explicit operator configuration use
    /// [`Self::launch_command_with_environment`]. Discovered metadata is never authority to
    /// inherit arbitrary secrets.
    ///
    /// # Errors
    ///
    /// Returns a structured refusal if the plugin was not discovered, is implementation-only
    /// metadata, or its manifest/executable identity changed since discovery.
    pub fn launch_command(&self, name: &str) -> Result<PinnedPluginCommand, RefusedPlugin> {
        self.launch_command_with_environment(name, &[])
    }

    /// Pin and revalidate a command with an explicit operator-controlled environment allowlist.
    ///
    /// The allowlist contains variable names only. Their values are read from the current process
    /// for this launch plan and are never retained in a manifest or durable plugin record. Every
    /// configured value must be available; the launch is refused before spawn otherwise.
    ///
    /// The manifest cannot request environment variables or arguments. Only the host's validated
    /// configuration may supply `environment_names`.
    ///
    /// # Errors
    ///
    /// Returns a structured refusal if the allowlist is invalid, a configured value is absent,
    /// the plugin was not discovered, or its manifest/executable identity changed since discovery.
    pub fn launch_command_with_environment(
        &self,
        name: &str,
        environment_names: &[String],
    ) -> Result<PinnedPluginCommand, RefusedPlugin> {
        validate_plugin_environment_names(environment_names)
            .map_err(|detail| refused(name, "invalid_environment_allowlist", detail))?;
        let configured_environment = environment_names
            .iter()
            .map(|key| {
                env::var_os(key)
                    .map(|value| (key.clone(), value))
                    .ok_or_else(|| {
                        refused(
                            name,
                            "environment_unavailable",
                            format!("configured plugin environment value {key:?} is unavailable"),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let plugin = self
            .discovered
            .iter()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| {
                refused(
                    name,
                    "plugin_not_discovered",
                    "plugin is not safely discovered",
                )
            })?;
        if !plugin.status.implemented {
            return Err(refused(
                name,
                "plugin_not_implemented",
                "plugin manifest does not report an implementation",
            ));
        }
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| refused(name, "home_unavailable", "plugin home is unavailable"))?;
        let uid = current_uid()?;
        let home_directory = open_validated_directory(home, uid, "plugin home")
            .map_err(|error| refused(name, "unsafe_plugin_home", error.to_string()))?;
        let root_path = descriptor_path(&home_directory).join("plugins");
        let root_directory = open_validated_directory(&root_path, uid, "plugin directory")
            .map_err(|error| refused(name, "unsafe_plugin_directory", error.to_string()))?;
        let directory_path = descriptor_path(&root_directory).join(&plugin.name);
        let directory = open_validated_directory(&directory_path, uid, "plugin entry")
            .map_err(|error| refused(name, "unsafe_plugin_entry", error.to_string()))?;
        let pinned_directory = descriptor_path(&directory);

        let manifest_bytes = read_manifest(&pinned_directory.join("manifest.json"), uid)
            .map_err(|error| refused(name, "invalid_manifest", error.to_string()))?;
        let (manifest, executable_name, process_phase_timeouts) =
            parse_manifest(&manifest_bytes, name)?;
        if manifest.name != plugin.name
            || manifest.capability != plugin.capability
            || executable_name != plugin.executable
            || manifest.protocol.min != plugin.protocol_min
            || manifest.protocol.max != plugin.protocol_max
            || (manifest.implementation == ImplementationState::Implemented)
                != plugin.status.implemented
            || process_phase_timeouts != plugin.process_phase_timeouts
        {
            return Err(refused(
                name,
                "plugin_identity_changed",
                "plugin manifest changed after discovery",
            ));
        }

        let executable_path = pinned_directory.join(&plugin.executable);
        let executable = open_validated_executable(&executable_path, uid)
            .map_err(|error| refused(name, "unsafe_executable", error.to_string()))?;
        let observed_identity = FileIdentity::from_metadata(
            &executable
                .metadata()
                .map_err(|error| refused(name, "unsafe_executable", error.to_string()))?,
        );
        if plugin.executable_identity.as_ref() != Some(&observed_identity) {
            return Err(refused(
                name,
                "plugin_identity_changed",
                "plugin executable changed after discovery",
            ));
        }

        let mut command = Command::new(descriptor_path(&executable));
        configure_plugin_environment(&mut command, &configured_environment);
        command.current_dir(pinned_directory);
        Ok(PinnedPluginCommand {
            command,
            process_phase_timeouts,
            _directory: directory,
            _executable: executable,
        })
    }
}

fn configure_plugin_environment(
    command: &mut Command,
    configured_environment: &[(String, OsString)],
) {
    command.env_clear();
    for key in PLUGIN_BASELINE_ENV {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, value) in configured_environment {
        command.env(key, value);
    }
}

pub(crate) fn validate_plugin_environment_names(
    environment_names: &[String],
) -> Result<(), String> {
    if environment_names.len() > MAX_PLUGIN_ENVIRONMENT_NAMES {
        return Err(format!(
            "plugin environment allowlist exceeds {MAX_PLUGIN_ENVIRONMENT_NAMES} names"
        ));
    }
    let mut seen = HashSet::with_capacity(environment_names.len());
    for name in environment_names {
        let mut bytes = name.bytes();
        let first = bytes.next();
        if name.len() > MAX_PLUGIN_ENVIRONMENT_NAME_BYTES
            || !matches!(first, Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(format!(
                "plugin environment name must be at most {MAX_PLUGIN_ENVIRONMENT_NAME_BYTES} bytes and match [A-Za-z_][A-Za-z0-9_]*"
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err("plugin environment names must be unique".to_owned());
        }
    }
    Ok(())
}

/// A launch plan whose directory and executable are held by open descriptors through `spawn`.
///
/// The executable path names its pinned descriptor through `/proc/self/fd`, so replacing the
/// installation directory after this value is returned cannot redirect execution. Plugins are
/// expected to be native executables; a script interpreter may be unable to reopen a descriptor
/// path after close-on-exec processing.
pub struct PinnedPluginCommand {
    command: Command,
    process_phase_timeouts: ProcessPhaseTimeouts,
    _directory: File,
    _executable: File,
}

impl fmt::Debug for PinnedPluginCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let environment_names = self
            .command
            .get_envs()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        formatter
            .debug_struct("PinnedPluginCommand")
            .field("program", &self.command.get_program())
            .field("argument_count", &self.command.get_args().count())
            .field("environment_names", &environment_names)
            .field("process_phase_timeouts", &self.process_phase_timeouts)
            .finish_non_exhaustive()
    }
}

impl PinnedPluginCommand {
    /// Return the validated manifest-owned process-protocol phase policy.
    #[must_use]
    pub(crate) fn process_phase_timeouts(&self) -> ProcessPhaseTimeouts {
        self.process_phase_timeouts
    }

    /// Spawn in a private process group with piped protocol streams and return its supervisor.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] when the pinned executable cannot be started.
    pub fn spawn(self) -> io::Result<SupervisedPluginChild> {
        chat_subscription_plugin::process::ProcessPluginChild::spawn(self.command)
    }

    /// Spawn and negotiate this pinned executable with bounded process-protocol phases.
    ///
    /// The returned cancellation handle independently interrupts a blocked event receive. Keep it
    /// for the complete subscription lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessPluginError`] after bounded process cleanup if spawn or Hello fails.
    pub fn connect(
        self,
        timeouts: ProcessPhaseTimeouts,
    ) -> Result<(ProcessPluginBackend, ProcessPluginCancellation), ProcessPluginError> {
        self.spawn()?.connect(timeouts)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ImplementationState {
    Planned,
    Implemented,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolManifest {
    name: String,
    min: u16,
    max: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProcessPhaseTimeoutManifest {
    hello_seconds: u64,
    start_seconds: u64,
    commit_seconds: u64,
    close_seconds: u64,
    shutdown_grace_seconds: u64,
}

impl Default for ProcessPhaseTimeoutManifest {
    fn default() -> Self {
        Self {
            hello_seconds: DEFAULT_PLUGIN_HELLO_TIMEOUT_SECONDS,
            start_seconds: DEFAULT_PLUGIN_START_TIMEOUT_SECONDS,
            commit_seconds: DEFAULT_PLUGIN_COMMIT_TIMEOUT_SECONDS,
            close_seconds: DEFAULT_PLUGIN_CLOSE_TIMEOUT_SECONDS,
            shutdown_grace_seconds: DEFAULT_PLUGIN_SHUTDOWN_GRACE_SECONDS,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginManifest {
    schema: String,
    name: String,
    capability: String,
    executable: String,
    protocol: ProtocolManifest,
    implementation: ImplementationState,
    #[serde(default)]
    process_phase_timeouts: ProcessPhaseTimeoutManifest,
}

fn bounded(value: impl Into<String>) -> String {
    let mut value = value.into();
    if value.len() > 2_000 {
        let mut boundary = 2_000;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

fn refused(
    entry: impl Into<String>,
    code: &'static str,
    detail: impl Into<String>,
) -> RefusedPlugin {
    RefusedPlugin {
        entry: bounded(entry),
        code,
        detail: bounded(detail),
    }
}

fn current_uid() -> Result<u32, RefusedPlugin> {
    fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|error| {
            refused(
                "plugins",
                "owner_unavailable",
                format!("cannot determine current process owner: {error}"),
            )
        })
}

fn resolve_plugin_home(
    agentctl_home: Option<OsString>,
    user_home: Option<OsString>,
) -> Option<PathBuf> {
    agentctl_home
        .map(PathBuf::from)
        .or_else(|| user_home.map(|home| PathBuf::from(home).join(".agentctl")))
}

/// Resolve the configured plugin home and inspect manifests without executing a plugin.
#[must_use]
pub fn discover() -> PluginInventory {
    let home = resolve_plugin_home(env::var_os(AGENTCTL_HOME_ENV), env::var_os("HOME"));
    match home {
        Some(home) => discover_at(home),
        None => PluginInventory {
            home: None,
            discovered: Vec::new(),
            refused: vec![refused(
                "plugins",
                "home_unavailable",
                "neither AGENTCTL_HOME nor HOME is set",
            )],
        },
    }
}

/// Inspect one explicit home. This is the deterministic entry point for tests and embedders.
#[must_use]
pub fn discover_at(home: PathBuf) -> PluginInventory {
    let mut inventory = PluginInventory {
        home: Some(home.clone()),
        discovered: Vec::new(),
        refused: Vec::new(),
    };
    if !home.is_absolute() {
        inventory.refused.push(refused(
            "plugins",
            "relative_home",
            "plugin home must be an absolute path",
        ));
        return inventory;
    }
    let uid = match current_uid() {
        Ok(uid) => uid,
        Err(error) => {
            inventory.refused.push(error);
            return inventory;
        }
    };
    if let Err(error) = validate_directory(&home, uid, "plugin home") {
        if error.kind() == std::io::ErrorKind::NotFound {
            return inventory;
        }
        inventory
            .refused
            .push(refused("plugins", "unsafe_plugin_home", error.to_string()));
        return inventory;
    }
    let root = home.join("plugins");
    if let Err(error) = validate_directory(&root, uid, "plugin directory") {
        if error.kind() == std::io::ErrorKind::NotFound {
            return inventory;
        }
        inventory.refused.push(refused(
            "plugins",
            "unsafe_plugin_directory",
            error.to_string(),
        ));
        return inventory;
    }
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            inventory.refused.push(refused(
                "plugins",
                "directory_unreadable",
                error.to_string(),
            ));
            return inventory;
        }
    };
    let mut paths = Vec::new();
    let mut entry_count = 0_usize;
    for entry in entries.take(MAX_PLUGIN_ENTRIES + 1) {
        entry_count += 1;
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(error) => {
                inventory
                    .refused
                    .push(refused("plugins", "entry_unreadable", error.to_string()))
            }
        }
    }
    if entry_count > MAX_PLUGIN_ENTRIES {
        inventory.discovered.clear();
        inventory.refused.push(refused(
            "plugins",
            "entry_limit_exceeded",
            format!("plugin directory exceeds {MAX_PLUGIN_ENTRIES} entries"),
        ));
        return inventory;
    }
    paths.sort();
    let mut candidates = Vec::new();
    for path in paths {
        let entry = path.file_name().map_or_else(
            || "<invalid>".to_owned(),
            |name| bounded(name.to_string_lossy()),
        );
        match inspect_plugin(&path, &entry, uid) {
            Ok(plugin) => candidates.push((entry, plugin)),
            Err(error) => inventory.refused.push(error),
        }
    }
    let mut capability_counts = HashMap::new();
    for (_, plugin) in &candidates {
        *capability_counts
            .entry(plugin.capability.clone())
            .or_insert(0_usize) += 1;
    }
    for (entry, plugin) in candidates {
        if plugin.capability == "chat-subscription.core" {
            inventory.refused.push(refused(
                entry,
                "duplicate_capability",
                "capability chat-subscription.core is reserved for the built-in core",
            ));
        } else if capability_counts[&plugin.capability] > 1 {
            inventory.refused.push(refused(
                entry,
                "duplicate_capability",
                format!(
                    "capability {} is declared by multiple plugin entries; all were refused",
                    plugin.capability
                ),
            ));
        } else {
            inventory.discovered.push(plugin);
        }
    }
    inventory
}

fn validate_directory_metadata(metadata: &fs::Metadata, uid: u32, label: &str) -> io::Result<()> {
    let mode = metadata.permissions().mode();
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || mode & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{label} must be a current-user-owned real directory without group/world write permission"
            ),
        ));
    }
    Ok(())
}

fn validate_directory(path: &Path, uid: u32, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_directory_metadata(&metadata, uid, label)
}

fn open_validated_directory(path: &Path, uid: u32, label: &str) -> io::Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_directory_metadata(&directory.metadata()?, uid, label)?;
    Ok(directory)
}

fn descriptor_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn valid_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn valid_capability(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('.').all(valid_slug)
        && value.contains('.')
}

fn inspect_plugin(path: &Path, entry: &str, uid: u32) -> Result<DiscoveredPlugin, RefusedPlugin> {
    validate_directory(path, uid, "plugin entry")
        .map_err(|error| refused(entry, "unsafe_plugin_entry", error.to_string()))?;
    if !valid_slug(entry) {
        return Err(refused(
            entry,
            "invalid_name",
            "plugin directory name must be a lowercase slug",
        ));
    }
    let manifest_path = path.join("manifest.json");
    let manifest_bytes = read_manifest(&manifest_path, uid)
        .map_err(|error| refused(entry, "invalid_manifest", error.to_string()))?;
    let (manifest, executable, process_phase_timeouts) = parse_manifest(&manifest_bytes, entry)?;
    let executable_metadata = validate_executable(&path.join(&executable), uid)
        .map_err(|error| refused(entry, "unsafe_executable", error.to_string()))?;
    Ok(DiscoveredPlugin {
        name: manifest.name,
        capability: manifest.capability,
        executable,
        protocol_name: chat_subscription_plugin::PROTOCOL_NAME,
        protocol_min: manifest.protocol.min,
        protocol_max: manifest.protocol.max,
        origin: "discovered",
        status: MaturityStatus::discovered(
            manifest.implementation == ImplementationState::Implemented,
        ),
        process_phase_timeouts,
        executable_identity: Some(FileIdentity::from_metadata(&executable_metadata)),
    })
}

fn parse_manifest(
    bytes: &[u8],
    entry: &str,
) -> Result<(PluginManifest, String, ProcessPhaseTimeouts), RefusedPlugin> {
    let manifest = chat_subscription_plugin::decode_strict_json::<PluginManifest>(bytes)
        .map_err(|error| refused(entry, "invalid_manifest", error.to_string()))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(refused(
            entry,
            "incompatible_manifest",
            format!(
                "manifest schema {:?} is unsupported; expected {MANIFEST_SCHEMA}",
                manifest.schema
            ),
        ));
    }
    if manifest.name != entry || !valid_slug(&manifest.name) {
        return Err(refused(
            entry,
            "name_mismatch",
            "manifest name must equal its lowercase-slug directory name",
        ));
    }
    if !valid_capability(&manifest.capability) {
        return Err(refused(
            entry,
            "invalid_capability",
            "capability must be a dotted lowercase slug of at most 128 bytes",
        ));
    }
    if manifest.protocol.name != chat_subscription_plugin::PROTOCOL_NAME
        || manifest.protocol.min == 0
        || manifest.protocol.min > manifest.protocol.max
        || !(manifest.protocol.min..=manifest.protocol.max)
            .contains(&chat_subscription_plugin::PROTOCOL_VERSION)
    {
        return Err(refused(
            entry,
            "incompatible_protocol",
            format!(
                "plugin protocol {:?} range {}-{} is incompatible with {} version {}",
                manifest.protocol.name,
                manifest.protocol.min,
                manifest.protocol.max,
                chat_subscription_plugin::PROTOCOL_NAME,
                chat_subscription_plugin::PROTOCOL_VERSION
            ),
        ));
    }
    let process_phase_timeouts =
        validate_process_phase_timeouts(entry, manifest.process_phase_timeouts)?;
    let executable = direct_child_name(&manifest.executable).ok_or_else(|| {
        refused(
            entry,
            "invalid_executable_path",
            "manifest executable must be one direct-child filename",
        )
    })?;
    Ok((manifest, executable, process_phase_timeouts))
}

fn validate_process_phase_timeouts(
    entry: &str,
    configured: ProcessPhaseTimeoutManifest,
) -> Result<ProcessPhaseTimeouts, RefusedPlugin> {
    for (name, value, maximum) in [
        (
            "hello_seconds",
            configured.hello_seconds,
            MAX_PLUGIN_HELLO_TIMEOUT_SECONDS,
        ),
        (
            "start_seconds",
            configured.start_seconds,
            MAX_PLUGIN_START_TIMEOUT_SECONDS,
        ),
        (
            "commit_seconds",
            configured.commit_seconds,
            MAX_PLUGIN_COMMIT_TIMEOUT_SECONDS,
        ),
        (
            "close_seconds",
            configured.close_seconds,
            MAX_PLUGIN_CLOSE_TIMEOUT_SECONDS,
        ),
    ] {
        if value == 0 || value > maximum {
            return Err(refused(
                entry,
                "invalid_process_phase_timeouts",
                format!("process_phase_timeouts.{name} must be between 1 and {maximum} seconds"),
            ));
        }
    }
    if configured.shutdown_grace_seconds > MAX_PLUGIN_SHUTDOWN_GRACE_SECONDS {
        return Err(refused(
            entry,
            "invalid_process_phase_timeouts",
            format!(
                "process_phase_timeouts.shutdown_grace_seconds must be between 0 and {MAX_PLUGIN_SHUTDOWN_GRACE_SECONDS} seconds"
            ),
        ));
    }
    ProcessPhaseTimeouts::new(
        Duration::from_secs(configured.hello_seconds),
        Duration::from_secs(configured.start_seconds),
        Duration::from_secs(configured.commit_seconds),
        Duration::from_secs(configured.close_seconds),
        Duration::from_secs(configured.shutdown_grace_seconds),
    )
    .map_err(|error| refused(entry, "invalid_process_phase_timeouts", error.to_string()))
}

fn direct_child_name(value: &str) -> Option<String> {
    let path = Path::new(value);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name != OsStr::new("manifest.json") => {
            name.to_str().map(str::to_owned)
        }
        _ => None,
    }
}

fn validate_file_metadata(metadata: &fs::Metadata, uid: u32, executable: bool) -> io::Result<()> {
    let mode = metadata.permissions().mode();
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || mode & 0o022 != 0
        || (executable && mode & 0o100 == 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file must be regular, singly linked, current-user-owned, and not group/world writable",
        ));
    }
    Ok(())
}

fn manifest_capacity(length: u64) -> io::Result<usize> {
    if length > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manifest exceeds {MAX_MANIFEST_BYTES} bytes"),
        ));
    }
    usize::try_from(length).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest length does not fit this platform",
        )
    })
}

fn read_manifest(path: &Path, uid: u32) -> io::Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    validate_file_metadata(&before, uid, false)?;
    manifest_capacity(before.len())?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let opened = file.metadata()?;
    validate_file_metadata(&opened, uid, false)?;
    let capacity = manifest_capacity(opened.len())?;
    if FileIdentity::from_metadata(&before) != FileIdentity::from_metadata(&opened) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest changed before it was opened",
        ));
    }
    let opened_identity = FileIdentity::from_metadata(&opened);
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("manifest exceeds {MAX_MANIFEST_BYTES} bytes"),
        ));
    }
    let after = file.metadata()?;
    validate_file_metadata(&after, uid, false)?;
    if opened_identity != FileIdentity::from_metadata(&after) || after.len() != bytes.len() as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest changed while it was read",
        ));
    }
    Ok(bytes)
}

fn validate_executable(path: &Path, uid: u32) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    validate_file_metadata(&metadata, uid, true)?;
    Ok(metadata)
}

fn open_validated_executable(path: &Path, uid: u32) -> io::Result<File> {
    let executable = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_file_metadata(&executable.metadata()?, uid, true)?;
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const TEST_REAP_TIMEOUT: Duration = Duration::from_secs(2);
    const TEST_OBSERVE_INTERVAL: Duration = Duration::from_millis(10);

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "agentctl-plugin-discovery-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("fixture home");
            fs::set_permissions(&path, Permissions::from_mode(0o700)).expect("private home");
            let plugins = path.join("plugins");
            fs::create_dir(&plugins).expect("fixture plugin root");
            fs::set_permissions(&plugins, Permissions::from_mode(0o700))
                .expect("private plugin root");
            Self { path }
        }

        fn plugin(&self, name: &str, capability: &str, extra: &str) {
            let directory = self.path.join("plugins").join(name);
            fs::create_dir(&directory).expect("fixture plugin directory");
            fs::set_permissions(&directory, Permissions::from_mode(0o700))
                .expect("private plugin directory");
            let executable = directory.join("backend");
            fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("fixture executable");
            fs::set_permissions(&executable, Permissions::from_mode(0o700))
                .expect("private executable");
            let manifest = format!(
                "{{\"schema\":\"agentctl-plugin-manifest/v1alpha1\",\"name\":\"{name}\",\"capability\":\"{capability}\",\"executable\":\"backend\",\"protocol\":{{\"name\":\"agentctl-chat-subscription\",\"min\":1,\"max\":1}},\"implementation\":\"implemented\"{extra}}}"
            );
            let path = directory.join("manifest.json");
            fs::write(&path, manifest).expect("fixture manifest");
            fs::set_permissions(path, Permissions::from_mode(0o600)).expect("private manifest");
        }

        fn native_plugin(&self, name: &str, capability: &str, executable_source: &Path) {
            self.plugin(name, capability, "");
            let executable = self.path.join("plugins").join(name).join("backend");
            fs::remove_file(&executable).expect("remove script fixture");
            fs::copy(executable_source, &executable).expect("copy native fixture executable");
            fs::set_permissions(executable, Permissions::from_mode(0o700))
                .expect("private native executable");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn assert_process_phase_timeouts(actual: ProcessPhaseTimeouts, expected_seconds: [u64; 5]) {
        assert_eq!(actual.hello(), Duration::from_secs(expected_seconds[0]));
        assert_eq!(actual.start(), Duration::from_secs(expected_seconds[1]));
        assert_eq!(actual.commit(), Duration::from_secs(expected_seconds[2]));
        assert_eq!(actual.close(), Duration::from_secs(expected_seconds[3]));
        assert_eq!(
            actual.shutdown_grace(),
            Duration::from_secs(expected_seconds[4])
        );
    }

    #[cfg(target_os = "linux")]
    fn process_is_running(pid: u32) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_whitespace().next())
            .is_some_and(|state| !matches!(state, "Z" | "X"))
    }

    #[cfg(target_os = "linux")]
    fn spawn_descendant_holding_protocol_output(
        fixture: &Fixture,
        name: &str,
    ) -> (SupervisedPluginChild, BufReader<File>, u32) {
        fixture.native_plugin(
            name,
            &format!("chat-subscription.fixture.{name}"),
            Path::new("/bin/sh"),
        );
        let inventory = discover_at(fixture.path.clone());
        let pinned = inventory
            .launch_command(name)
            .expect("pin shell process fixture");
        let mut child = pinned.spawn().expect("spawn shell process fixture");
        let (reader, mut writer) = child.take_transport().expect("take protocol transport");
        writer
            .write_all(b"/bin/sleep 60 & printf '%s\\n' \"$!\"; exit\n")
            .expect("start descendant holding protocol output");
        writer.flush().expect("flush fixture command");
        drop(writer);

        let mut reader = BufReader::new(reader);
        let mut descendant_pid = String::new();
        reader
            .read_line(&mut descendant_pid)
            .expect("read descendant pid");
        let descendant_pid = descendant_pid
            .trim()
            .parse::<u32>()
            .expect("fixture reports a numeric descendant pid");
        let executable_id = child.executable_id();
        let executable_deadline = Instant::now() + Duration::from_secs(1);
        while process_is_running(executable_id) && Instant::now() < executable_deadline {
            thread::sleep(TEST_OBSERVE_INTERVAL);
        }
        assert!(
            !process_is_running(executable_id),
            "fixture executable must exit before descendant cleanup"
        );
        let executable_status = child
            .executable_status()
            .expect("observe fixture executable without reaping")
            .expect("fixture executable has exited");
        assert!(
            executable_status.success(),
            "fixture executable exited unsuccessfully: {executable_status}"
        );
        assert!(
            process_is_running(child.id()),
            "atomic supervisor must remain live to pin the private process-group identity"
        );
        assert!(
            process_is_running(descendant_pid),
            "fixture descendant must outlive its executable plugin"
        );
        (child, reader, descendant_pid)
    }

    #[cfg(target_os = "linux")]
    fn assert_descendant_died_and_protocol_output_closed(
        mut reader: BufReader<File>,
        descendant_pid: u32,
    ) {
        let (sender, receiver) = mpsc::channel();
        let reader_thread = thread::spawn(move || {
            let mut trailing_output = Vec::new();
            let result = reader
                .read_to_end(&mut trailing_output)
                .map(|_| trailing_output);
            let _ = sender.send(result);
        });

        let deadline = Instant::now() + TEST_REAP_TIMEOUT;
        while process_is_running(descendant_pid) && Instant::now() < deadline {
            thread::sleep(TEST_OBSERVE_INTERVAL);
        }
        let descendant_died = !process_is_running(descendant_pid);
        let pipe_result = receiver.recv_timeout(TEST_REAP_TIMEOUT);
        let pipe_closed = matches!(&pipe_result, Ok(Ok(_)));
        if !descendant_died || !pipe_closed {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &descendant_pid.to_string()])
                .status();
        }
        if pipe_result.is_err() {
            let _ = receiver.recv_timeout(TEST_REAP_TIMEOUT);
        }
        let _ = reader_thread.join();

        assert!(
            descendant_died,
            "supervisor left descendant {descendant_pid} running"
        );
        assert!(
            pipe_closed,
            "descendant kept protocol output open after supervision: {pipe_result:?}"
        );
    }

    #[test]
    fn environment_override_precedes_user_home() {
        assert_eq!(
            resolve_plugin_home(
                Some(OsString::from("/override")),
                Some(OsString::from("/home/user"))
            ),
            Some(PathBuf::from("/override"))
        );
        assert_eq!(
            resolve_plugin_home(None, Some(OsString::from("/home/user"))),
            Some(PathBuf::from("/home/user/.agentctl"))
        );
    }

    #[test]
    fn valid_manifest_reports_installation_without_runtime_claims() {
        let fixture = Fixture::new();
        fixture.plugin("fixture-chat", "chat-subscription.fixture", "");
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.refused, []);
        assert_eq!(inventory.discovered.len(), 1);
        let plugin = &inventory.discovered[0];
        assert_eq!(plugin.origin, "discovered");
        assert_eq!(plugin.protocol_name, "agentctl-chat-subscription");
        assert_eq!(
            plugin.status,
            MaturityStatus {
                implemented: true,
                configured: false,
                connected: false,
                live_verified: false,
            }
        );
        let pinned = inventory
            .launch_command("fixture-chat")
            .expect("safe plugin command");
        let command = &pinned.command;
        let current_directory = command.get_current_dir().expect("pinned current directory");
        assert_eq!(current_directory.parent(), Some(Path::new("/proc/self/fd")));
        assert_eq!(
            fs::canonicalize(current_directory).expect("resolve pinned current directory"),
            fs::canonicalize(fixture.path.join("plugins/fixture-chat"))
                .expect("resolve fixture plugin directory")
        );
        let environment = command
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(environment
            .iter()
            .all(|key| PLUGIN_BASELINE_ENV.contains(&key.as_str())));
        assert!(!environment.iter().any(|key| {
            matches!(
                key.as_str(),
                "OPENAI_API_KEY" | "GOOGLE_API_KEY" | "AWS_SECRET_ACCESS_KEY"
            )
        }));
    }

    #[test]
    fn legacy_manifest_keeps_exact_process_phase_timeout_defaults() {
        let fixture = Fixture::new();
        fixture.plugin("legacy-chat", "chat-subscription.legacy", "");
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.refused, []);
        assert_eq!(inventory.discovered.len(), 1);
        assert_process_phase_timeouts(
            inventory.discovered[0].process_phase_timeouts,
            [10, 30, 30, 10, 2],
        );
        let pinned = inventory
            .launch_command("legacy-chat")
            .expect("pin legacy plugin");
        assert_process_phase_timeouts(pinned.process_phase_timeouts(), [10, 30, 30, 10, 2]);
    }

    #[test]
    fn bounded_manifest_phase_timeouts_propagate_to_pinned_launch_plan() {
        let fixture = Fixture::new();
        fixture.plugin(
            "bounded-chat",
            "chat-subscription.bounded",
            ",\"process_phase_timeouts\":{\"hello_seconds\":20,\"start_seconds\":130,\"commit_seconds\":45,\"close_seconds\":57,\"shutdown_grace_seconds\":0}",
        );
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.refused, []);
        assert_eq!(inventory.discovered.len(), 1);
        assert_process_phase_timeouts(
            inventory.discovered[0].process_phase_timeouts,
            [20, 130, 45, 57, 0],
        );
        let pinned = inventory
            .launch_command("bounded-chat")
            .expect("pin bounded plugin");
        assert_process_phase_timeouts(pinned.process_phase_timeouts(), [20, 130, 45, 57, 0]);
    }

    #[test]
    fn manifest_process_phase_timeout_boundaries_are_accepted() {
        let fixture = Fixture::new();
        fixture.plugin(
            "boundary-chat",
            "chat-subscription.boundary",
            ",\"process_phase_timeouts\":{\"hello_seconds\":30,\"start_seconds\":180,\"commit_seconds\":60,\"close_seconds\":120,\"shutdown_grace_seconds\":10}",
        );
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.refused, []);
        assert_process_phase_timeouts(
            inventory.discovered[0].process_phase_timeouts,
            [30, 180, 60, 120, 10],
        );
    }

    #[test]
    fn manifest_process_phase_timeout_boundaries_are_refused_before_launch() {
        let violations = [
            ("zero-hello", "0,\"start_seconds\":30,\"commit_seconds\":30,\"close_seconds\":10,\"shutdown_grace_seconds\":2"),
            ("large-hello", "31,\"start_seconds\":30,\"commit_seconds\":30,\"close_seconds\":10,\"shutdown_grace_seconds\":2"),
            ("zero-start", "10,\"start_seconds\":0,\"commit_seconds\":30,\"close_seconds\":10,\"shutdown_grace_seconds\":2"),
            ("large-start", "10,\"start_seconds\":181,\"commit_seconds\":30,\"close_seconds\":10,\"shutdown_grace_seconds\":2"),
            ("zero-commit", "10,\"start_seconds\":30,\"commit_seconds\":0,\"close_seconds\":10,\"shutdown_grace_seconds\":2"),
            ("large-commit", "10,\"start_seconds\":30,\"commit_seconds\":61,\"close_seconds\":10,\"shutdown_grace_seconds\":2"),
            ("zero-close", "10,\"start_seconds\":30,\"commit_seconds\":30,\"close_seconds\":0,\"shutdown_grace_seconds\":2"),
            ("large-close", "10,\"start_seconds\":30,\"commit_seconds\":30,\"close_seconds\":121,\"shutdown_grace_seconds\":2"),
            ("large-grace", "10,\"start_seconds\":30,\"commit_seconds\":30,\"close_seconds\":10,\"shutdown_grace_seconds\":11"),
        ];
        for (name, fields) in violations {
            let fixture = Fixture::new();
            fixture.plugin(
                name,
                &format!("chat-subscription.{name}"),
                &format!(",\"process_phase_timeouts\":{{\"hello_seconds\":{fields}}}"),
            );
            let inventory = discover_at(fixture.path.clone());
            assert!(inventory.discovered.is_empty(), "{name} was discovered");
            assert_eq!(inventory.refused.len(), 1, "{name} refusal count");
            assert_eq!(
                inventory.refused[0].code, "invalid_process_phase_timeouts",
                "{name} refusal: {:?}",
                inventory.refused[0]
            );
        }
    }

    #[test]
    fn manifest_process_phase_timeout_object_is_strict_and_complete() {
        for (name, fields) in [
            (
                "missing-close",
                "\"hello_seconds\":10,\"start_seconds\":30,\"commit_seconds\":30,\"shutdown_grace_seconds\":2",
            ),
            (
                "unknown-unit",
                "\"hello_seconds\":10,\"start_seconds\":30,\"commit_seconds\":30,\"close_seconds\":10,\"shutdown_grace_seconds\":2,\"close_millis\":10000",
            ),
        ] {
            let fixture = Fixture::new();
            fixture.plugin(
                name,
                &format!("chat-subscription.{name}"),
                &format!(",\"process_phase_timeouts\":{{{fields}}}"),
            );
            let inventory = discover_at(fixture.path.clone());
            assert!(inventory.discovered.is_empty(), "{name} was discovered");
            assert_eq!(inventory.refused.len(), 1, "{name} refusal count");
            assert_eq!(inventory.refused[0].code, "invalid_manifest", "{name}");
        }
    }

    #[test]
    fn operator_environment_is_added_after_a_cleared_baseline() {
        let secret = OsString::from("not-persisted-provider-secret");
        let mut command = Command::new("/bin/true");
        command.env("MUST_BE_CLEARED", "unexpected");
        configure_plugin_environment(
            &mut command,
            &[("PROVIDER_CREDENTIAL".to_owned(), secret.clone())],
        );
        let environment = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.expect("configured environment value").to_os_string(),
                )
            })
            .collect::<HashMap<_, _>>();
        assert!(!environment.contains_key("MUST_BE_CLEARED"));
        assert_eq!(environment.get("PROVIDER_CREDENTIAL"), Some(&secret));
        assert!(environment.keys().all(|key| {
            key == "PROVIDER_CREDENTIAL" || PLUGIN_BASELINE_ENV.contains(&key.as_str())
        }));
    }

    #[test]
    fn pinned_command_debug_reports_environment_names_without_values() {
        let mut command = Command::new("/bin/true");
        command
            .env_clear()
            .env("PROVIDER_CREDENTIAL", "not-persisted-provider-secret");
        let directory = File::open("/dev/null").expect("fixture descriptor");
        let executable = directory.try_clone().expect("fixture clone");
        let pinned = PinnedPluginCommand {
            command,
            process_phase_timeouts: validate_process_phase_timeouts(
                "fixture",
                ProcessPhaseTimeoutManifest::default(),
            )
            .expect("default phase timeouts"),
            _directory: directory,
            _executable: executable,
        };
        let debug = format!("{pinned:?}");
        assert!(debug.contains("PROVIDER_CREDENTIAL"));
        assert!(!debug.contains("not-persisted-provider-secret"));
    }

    #[test]
    fn invalid_or_unavailable_operator_environment_is_refused_before_spawn() {
        let fixture = Fixture::new();
        fixture.plugin("fixture-chat", "chat-subscription.fixture", "");
        let inventory = discover_at(fixture.path.clone());
        for names in [
            vec!["BAD-NAME".to_owned()],
            vec!["DUPLICATE".to_owned(), "DUPLICATE".to_owned()],
            vec!["X".repeat(MAX_PLUGIN_ENVIRONMENT_NAME_BYTES + 1)],
            vec!["X".to_owned(); MAX_PLUGIN_ENVIRONMENT_NAMES + 1],
        ] {
            let refusal = inventory
                .launch_command_with_environment("fixture-chat", &names)
                .expect_err("invalid environment allowlist must be refused");
            assert_eq!(refusal.code, "invalid_environment_allowlist");
        }
        let refusal = inventory
            .launch_command_with_environment(
                "fixture-chat",
                &["AGENTCTL_TEST_DEFINITELY_UNAVAILABLE_713C".to_owned()],
            )
            .expect_err("missing configured value must be refused");
        assert_eq!(refusal.code, "environment_unavailable");
    }

    #[test]
    fn manifest_cannot_select_environment_variables() {
        let fixture = Fixture::new();
        fixture.plugin(
            "environment-chat",
            "chat-subscription.environment",
            ",\"environment\":[\"PROVIDER_CREDENTIAL\"]",
        );
        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(inventory.refused.len(), 1);
        assert_eq!(inventory.refused[0].code, "invalid_manifest");
        assert!(inventory.refused[0].detail.contains("unknown field"));
    }

    #[test]
    fn manifest_cannot_claim_connection_or_live_verification() {
        let fixture = Fixture::new();
        fixture.plugin(
            "lying-chat",
            "chat-subscription.lying",
            ",\"connected\":true,\"live_verified\":true",
        );
        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(inventory.refused.len(), 1);
        assert_eq!(inventory.refused[0].code, "invalid_manifest");
    }

    #[test]
    fn manifest_duplicate_keys_are_refused_instead_of_using_last_value() {
        let fixture = Fixture::new();
        fixture.plugin("duplicate-chat", "chat-subscription.duplicate", "");
        let manifest = fixture.path.join("plugins/duplicate-chat/manifest.json");
        let text = fs::read_to_string(&manifest)
            .expect("fixture manifest")
            .replace(
                "\"name\":\"duplicate-chat\"",
                "\"name\":\"duplicate-chat\",\"name\":\"duplicate-chat\"",
            );
        fs::write(&manifest, text).expect("rewrite duplicate manifest");
        fs::set_permissions(&manifest, Permissions::from_mode(0o600))
            .expect("private duplicate manifest");
        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(inventory.refused.len(), 1);
        assert_eq!(inventory.refused[0].code, "invalid_manifest");
        assert!(inventory.refused[0]
            .detail
            .contains("duplicate JSON object key"));
    }

    #[test]
    fn incompatible_and_duplicate_capabilities_are_visible_refusals() {
        let fixture = Fixture::new();
        fixture.plugin("alpha-chat", "chat-subscription.shared", "");
        fixture.plugin("beta-chat", "chat-subscription.shared", "");
        fixture.plugin("old-chat", "chat-subscription.old", "");
        let old_manifest = fixture.path.join("plugins/old-chat/manifest.json");
        let text = fs::read_to_string(&old_manifest)
            .expect("old manifest")
            .replace("\"min\":1,\"max\":1", "\"min\":9,\"max\":10");
        fs::write(&old_manifest, text).expect("rewrite old manifest");
        fs::set_permissions(&old_manifest, Permissions::from_mode(0o600))
            .expect("private old manifest");
        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(inventory.refused.len(), 3);
        assert_eq!(
            inventory
                .refused
                .iter()
                .filter(|item| item.code == "duplicate_capability")
                .count(),
            2
        );
        assert!(inventory
            .refused
            .iter()
            .any(|item| item.code == "duplicate_capability"));
        assert!(inventory
            .refused
            .iter()
            .any(|item| item.code == "incompatible_protocol"));
    }

    #[test]
    fn provider_implementation_capabilities_can_coinstall_without_shadowing() {
        let fixture = Fixture::new();
        fixture.plugin(
            "workspace-events",
            "chat-subscription.google-chat.workspace-events",
            "",
        );
        fixture.plugin("polling", "chat-subscription.google-chat.polling", "");
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.refused, []);
        assert_eq!(inventory.discovered.len(), 2);
        assert_eq!(inventory.discovered[0].name, "polling");
        assert_eq!(inventory.discovered[1].name, "workspace-events");
    }

    #[test]
    fn built_in_capability_cannot_be_shadowed() {
        let fixture = Fixture::new();
        fixture.plugin("shadow-chat", "chat-subscription.core", "");
        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(inventory.refused.len(), 1);
        assert_eq!(inventory.refused[0].code, "duplicate_capability");
        assert!(inventory.refused[0].detail.contains("reserved"));
    }

    #[test]
    fn symlinked_executable_and_path_escape_are_refused() {
        let fixture = Fixture::new();
        fixture.plugin("link-chat", "chat-subscription.link", "");
        let executable = fixture.path.join("plugins/link-chat/backend");
        fs::remove_file(&executable).expect("remove fixture executable");
        std::os::unix::fs::symlink("/bin/true", &executable).expect("fixture symlink");

        fixture.plugin("escape-chat", "chat-subscription.escape", "");
        let manifest = fixture.path.join("plugins/escape-chat/manifest.json");
        let text = fs::read_to_string(&manifest)
            .expect("escape manifest")
            .replace(
                "\"executable\":\"backend\"",
                "\"executable\":\"../backend\"",
            );
        fs::write(&manifest, text).expect("rewrite escape manifest");
        fs::set_permissions(&manifest, Permissions::from_mode(0o600))
            .expect("private escape manifest");

        fixture.plugin("hard-chat", "chat-subscription.hard", "");
        let hard_executable = fixture.path.join("plugins/hard-chat/backend");
        fs::hard_link(
            &hard_executable,
            fixture.path.join("plugins/hard-chat/second-link"),
        )
        .expect("fixture hard link");

        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(
            inventory
                .refused
                .iter()
                .filter(|item| item.code == "unsafe_executable")
                .count(),
            2
        );
        assert!(inventory
            .refused
            .iter()
            .any(|item| item.code == "invalid_executable_path"));
    }

    #[test]
    fn unsafe_home_root_and_entry_modes_are_refused() {
        let home = Fixture::new();
        fs::set_permissions(&home.path, Permissions::from_mode(0o777)).expect("unsafe home mode");
        let inventory = discover_at(home.path.clone());
        assert_eq!(inventory.refused[0].code, "unsafe_plugin_home");

        let root = Fixture::new();
        fs::set_permissions(root.path.join("plugins"), Permissions::from_mode(0o777))
            .expect("unsafe root mode");
        let inventory = discover_at(root.path.clone());
        assert_eq!(inventory.refused[0].code, "unsafe_plugin_directory");

        let entry = Fixture::new();
        entry.plugin("unsafe-entry", "chat-subscription.fixture.entry", "");
        fs::set_permissions(
            entry.path.join("plugins/unsafe-entry"),
            Permissions::from_mode(0o777),
        )
        .expect("unsafe entry mode");
        let inventory = discover_at(entry.path.clone());
        assert_eq!(inventory.refused[0].code, "unsafe_plugin_entry");
    }

    #[test]
    fn ownership_checks_reject_metadata_for_another_uid() {
        let fixture = Fixture::new();
        fixture.plugin("owner-chat", "chat-subscription.fixture.owner", "");
        let foreign_uid = fs::metadata("/proc/self").expect("process metadata").uid() ^ 1;
        assert!(validate_directory(&fixture.path, foreign_uid, "fixture home").is_err());
        let manifest = fs::metadata(fixture.path.join("plugins/owner-chat/manifest.json"))
            .expect("manifest metadata");
        assert!(validate_file_metadata(&manifest, foreign_uid, false).is_err());
    }

    #[test]
    fn entry_limit_fails_closed_before_any_manifest_is_accepted() {
        let fixture = Fixture::new();
        for index in 0..=MAX_PLUGIN_ENTRIES {
            let path = fixture
                .path
                .join("plugins")
                .join(format!("entry-{index:03}"));
            fs::create_dir(path).expect("fixture entry");
        }
        let inventory = discover_at(fixture.path.clone());
        assert!(inventory.discovered.is_empty());
        assert_eq!(inventory.refused.len(), 1);
        assert_eq!(inventory.refused[0].code, "entry_limit_exceeded");
    }

    #[test]
    fn manifest_capacity_refuses_one_byte_over_before_allocation() {
        assert_eq!(
            manifest_capacity(MAX_MANIFEST_BYTES).expect("exact bound fits"),
            usize::try_from(MAX_MANIFEST_BYTES).expect("manifest bound fits usize")
        );
        let error = manifest_capacity(MAX_MANIFEST_BYTES + 1)
            .expect_err("one byte over the manifest bound is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(&MAX_MANIFEST_BYTES.to_string()));
    }

    #[test]
    fn launch_revalidates_executable_after_discovery() {
        let fixture = Fixture::new();
        fixture.plugin("changed-chat", "chat-subscription.fixture.changed", "");
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.discovered.len(), 1);
        fs::set_permissions(
            fixture.path.join("plugins/changed-chat/backend"),
            Permissions::from_mode(0o777),
        )
        .expect("make executable unsafe after discovery");
        let error = inventory
            .launch_command("changed-chat")
            .expect_err("launch must revalidate executable mode");
        assert_eq!(error.code, "unsafe_executable");
    }

    #[test]
    fn launch_refuses_phase_timeout_change_after_discovery() {
        let fixture = Fixture::new();
        fixture.plugin("changed-chat", "chat-subscription.fixture.changed", "");
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.discovered.len(), 1);
        let manifest = fixture.path.join("plugins/changed-chat/manifest.json");
        let text = fs::read_to_string(&manifest)
            .expect("fixture manifest")
            .replace(
                "\"implementation\":\"implemented\"",
                "\"implementation\":\"implemented\",\"process_phase_timeouts\":{\"hello_seconds\":20,\"start_seconds\":130,\"commit_seconds\":45,\"close_seconds\":57,\"shutdown_grace_seconds\":0}",
            );
        fs::write(&manifest, text).expect("rewrite phase timeout manifest");
        fs::set_permissions(&manifest, Permissions::from_mode(0o600))
            .expect("private phase timeout manifest");

        let error = inventory
            .launch_command("changed-chat")
            .expect_err("phase timeout replacement after discovery must be refused");
        assert_eq!(error.code, "plugin_identity_changed");
    }

    #[test]
    fn launch_refuses_atomic_entry_replacement_before_pinning() {
        let fixture = Fixture::new();
        fixture.native_plugin(
            "replaced-chat",
            "chat-subscription.fixture.replaced",
            Path::new("/bin/true"),
        );
        let inventory = discover_at(fixture.path.clone());
        assert_eq!(inventory.discovered.len(), 1);

        let plugin_root = fixture.path.join("plugins");
        fs::rename(
            plugin_root.join("replaced-chat"),
            plugin_root.join("replaced-chat-old"),
        )
        .expect("move discovered entry aside");
        fixture.native_plugin(
            "replaced-chat",
            "chat-subscription.fixture.replaced",
            Path::new("/bin/false"),
        );

        let error = inventory
            .launch_command("replaced-chat")
            .expect_err("replacement after discovery must not redirect launch");
        assert_eq!(error.code, "plugin_identity_changed");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_command_survives_atomic_entry_replacement_after_return() {
        let fixture = Fixture::new();
        fixture.native_plugin(
            "pinned-chat",
            "chat-subscription.fixture.pinned",
            Path::new("/bin/true"),
        );
        let inventory = discover_at(fixture.path.clone());
        let pinned = inventory
            .launch_command("pinned-chat")
            .expect("pin discovered true executable");

        let plugin_root = fixture.path.join("plugins");
        fs::rename(
            plugin_root.join("pinned-chat"),
            plugin_root.join("pinned-chat-old"),
        )
        .expect("move pinned entry aside");
        fixture.native_plugin(
            "pinned-chat",
            "chat-subscription.fixture.pinned",
            Path::new("/bin/false"),
        );

        let mut child = pinned.spawn().expect("spawn pinned executable");
        let status = child
            .shutdown(Duration::from_secs(1))
            .expect("pinned executable exits and is reaped");
        assert!(
            status.success(),
            "replacement false executable ran: {status}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn supervisor_kills_and_reaps_a_blocked_plugin_within_its_fixed_bound() {
        let fixture = Fixture::new();
        fixture.native_plugin(
            "blocked-chat",
            "chat-subscription.fixture.blocked",
            Path::new("/bin/cat"),
        );
        let inventory = discover_at(fixture.path.clone());
        let pinned = inventory
            .launch_command("blocked-chat")
            .expect("pin blocking executable");
        let mut child = pinned.spawn().expect("spawn blocking executable");
        let (_reader, _writer) = child.take_transport().expect("take protocol transport");
        let status = child
            .shutdown(Duration::from_millis(20))
            .expect("blocked child is killed and reaped");
        assert!(!status.success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shutdown_kills_descendant_after_plugin_exits_while_supervisor_pins_group() {
        let fixture = Fixture::new();
        let (mut child, reader, descendant_pid) =
            spawn_descendant_holding_protocol_output(&fixture, "shutdown-tree");

        let status = child
            .shutdown(Duration::ZERO)
            .expect("shutdown terminates the complete plugin process group");
        assert!(
            status.success(),
            "fixture leader exited unsuccessfully: {status}"
        );
        assert_descendant_died_and_protocol_output_closed(reader, descendant_pid);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drop_kills_descendant_after_plugin_exits_while_supervisor_pins_group() {
        let fixture = Fixture::new();
        let (child, reader, descendant_pid) =
            spawn_descendant_holding_protocol_output(&fixture, "drop-tree");

        drop(child);
        assert_descendant_died_and_protocol_output_closed(reader, descendant_pid);
    }

    #[test]
    fn bounded_diagnostics_preserve_utf8_boundaries() {
        let value = bounded(format!("{}😀", "x".repeat(1_999)));
        assert_eq!(value.len(), 1_999);
        assert!(value.chars().all(|character| character == 'x'));
    }
}
