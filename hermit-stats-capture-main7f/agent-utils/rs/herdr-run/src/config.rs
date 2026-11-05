//! Strict per-project configuration discovery, parsing, and defaults.
//!
//! Every message in this module is part of a catalogue the other edition repeats word for word:
//! type names use the JSON vocabulary (`null`, `bool`, `number`, `string`, `array`, `object`),
//! values are quoted with [`crate::textfmt::quote`], and keys are validated in [`KNOWN_KEYS`]
//! order, each completely at its own position, so a file with two mistakes reports the same first
//! mistake in both editions. The file itself is read with herdr-run's own YAML-subset parser,
//! [`crate::yaml`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::{HerdrRunError, Result};
use crate::textfmt::{display, name_violation, os_error_text, quote, whole_milliseconds};
use crate::yaml::{core_load, Yaml};
/// Accepted configuration basenames, in preference order at each directory.
pub const CONFIG_FILENAMES: [&str; 2] = [".herdr-run.yaml", ".herdr-run.yml"];

/// The single `allow` entry that turns the allowlist off: any bare program name is admitted.
///
/// This is a named mode rather than something to be worked out, because the alternative — writing
/// out every program a project might ever want — is the kind of list people abandon halfway and
/// then widen by accident. It is deliberately the only entry permitted when it is present: an
/// `allow` reading `["*", "git"]` looks narrower than it is.
///
/// Everything except the program-name check still applies in this mode: terminal control
/// characters are still refused, the program must still be a bare name resolved from the pane's
/// `PATH`, wrapper prefixes must still be declared, and every `deny_*` rule still bites.
pub const ALLOW_ANY_PROGRAM: &str = "*";

/// Default ceiling on panes in the command workspace before a NEW tab replaces the least-recently-
/// used idle tab, or is refused when every tab is busy.
///
/// Every agent that ever runs a command leaves a tab behind and nothing else closes it, so without a
/// ceiling the workspace grows for as long as agents are coined. The number is not arbitrary:
/// measured on devbig014 2026-08-10, a session with 260 panes drove the Herdr server to >1000% CPU
/// with every control call timing out. 32 keeps an eightfold margin below that while being far
/// more tabs than one project's agents legitimately need, and the cap is per workspace, so several
/// projects on one server each want headroom of their own.
pub const DEFAULT_MAX_PANES: u64 = 32;

/// Largest accepted `max_panes`. A shared finite bound keeps the two implementations identical.
pub const MAX_PANE_CAP: u64 = 1_000_000;

/// Cargo options refused in every argument position whatever `deny_global` says.
///
/// A project that opts Cargo in must not be able to configure its way past them. The admission
/// rule ([`crate::allowlist::admit`]) adds these to Cargo's `deny_global` entries, and
/// [`missing_defaults`] therefore never reports them as left out.
pub const CARGO_ALWAYS_DENIED: [&str; 2] = ["--config", "-Z"];

/// Largest command/readiness timeout accepted from configuration or the CLI.
///
/// This deliberately stays far below platform monotonic-clock limits so a finite timeout cannot
/// overflow into an accidental infinite wait.
pub const MAX_TIMEOUT_SECONDS: f64 = 31_536_000.0;

/// Fully resolved `herdr-run` configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Herdr workspace label holding this project's command tabs.
    pub workspace: String,
    /// Tab-label format supporting `{agent}` and `{project}`.
    pub tab_name: String,
    /// Optional command working directory; `None` leaves execution at the caller's directory.
    pub cwd: Option<String>,
    /// Bare program names admitted by policy.
    pub allow: Vec<String>,
    /// Wrapper programs that may precede an allowlisted program.
    pub prefixes: Vec<String>,
    /// Program-specific global options; Cargo entries are denied in every argument position.
    pub deny_global: BTreeMap<String, Vec<String>>,
    /// Program-specific subcommands denied by policy.
    pub deny_subcommand: BTreeMap<String, Vec<String>>,
    /// Options denied wherever they occur after the program.
    pub deny_anywhere: Vec<String>,
    /// Options denied after one subcommand: program, then subcommand, then options.
    ///
    /// For an option that names a program only there, while the same letters are harmless
    /// elsewhere: `git clone -u CMD` is `--upload-pack`, `git push -u` is not.
    pub deny_subcommand_option: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// Programs whose subcommand must appear in the corresponding positive list.
    pub allow_subcommand: BTreeMap<String, Vec<String>>,
    /// Global options whose value occupies the following token: the built-in table plus whatever
    /// the configuration adds.
    ///
    /// The table records how each program reads its own command line, which is a fact about the
    /// program and not a policy choice, so a configured `value_options` ADDS to it and can never
    /// take an entry away. Leaving an entry out could only make the policy misread a command -- and
    /// a file written by an older `herdr-run init` lists the older, shorter table, which used to
    /// replace this one and so reopened `git --attr-source HEAD daemon`.
    pub value_options: BTreeMap<String, Vec<String>>,
    /// Directory holding run spools, audit log, and session cache.
    pub spool_dir: String,
    /// Maximum seconds to wait for a launched command to complete.
    pub timeout_seconds: f64,
    /// Days of per-run spool output to retain, pruned when a new run is written.
    pub retention_days: u64,
    /// Ceiling on panes in `workspace` before a NEW tab is refused; `0` disables the cap.
    ///
    /// Checked only when a tab has to be created, so an agent that already has its tab is never
    /// locked out of it. A cap that can break work in progress is a cap that gets switched off.
    pub max_panes: u64,
    /// Maximum seconds to wait for a pane to become ready.
    pub ready_timeout_seconds: f64,
    /// Readiness policy: `both` or `process`.
    pub readiness: String,
    /// Explicit rendered prompt suffix, or `None` to infer it.
    pub prompt_tail: Option<String>,
    /// Process names accepted as interactive shells.
    pub shells: Vec<String>,
    /// Remote used by the two-direction `doctor` probe.
    pub probe_remote: String,
    /// Herdr control-call broker: `direct` or `systemd-run`.
    pub broker: String,
    /// Absolute source configuration path, if a file supplied this configuration.
    pub source_path: Option<String>,
    /// Absolute project root used to resolve relative paths.
    pub project_root: String,
}

impl Config {
    /// Report whether `allow` has been set to the [`ALLOW_ANY_PROGRAM`] wildcard.
    #[must_use]
    pub fn allows_any_program(&self) -> bool {
        self.allow.iter().any(|entry| entry == ALLOW_ANY_PROGRAM)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: "agent-cmds".to_owned(),
            tab_name: "{agent}".to_owned(),
            cwd: None,
            // Cargo is deliberately not a default: even dependency-oriented subcommands can
            // execute configured rustc wrappers, credential providers, or fetch helpers. The
            // cargo-specific policy below limits projects that explicitly accept that widening.
            allow: strings(&["git", "gh"]),
            prefixes: strings(&["with-proxy"]),
            deny_global: string_map(&[
                ("git", &["-c", "--config-env", "--exec-path", "--namespace"]),
                ("gh", &[]),
                ("cargo", &["--config", "-Z"]),
            ]),
            deny_subcommand: string_map(&[
                ("git", &["filter-branch", "daemon", "instaweb"]),
                ("gh", &["alias", "extension", "ext", "codespace", "cs"]),
            ]),
            // `--exec` is the older spelling of `--upload-pack`/`--receive-pack` (push, ls-remote,
            // fetch-pack, send-pack) and names the program `git archive --remote` runs.
            deny_anywhere: strings(&["--upload-pack", "--receive-pack", "--exec"]),
            // `git clone -u CMD` is `--upload-pack`; `git rebase -x CMD` runs CMD after each
            // commit; `git difftool -x CMD` (`--extcmd`) runs CMD as the diff tool.
            deny_subcommand_option: BTreeMap::from([(
                "git".to_owned(),
                string_map(&[
                    ("clone", &["-u"]),
                    ("difftool", &["-x", "--extcmd"]),
                    ("rebase", &["-x"]),
                ]),
            )]),
            allow_subcommand: string_map(&[(
                "cargo",
                &[
                    "fetch",
                    "update",
                    "generate-lockfile",
                    "vendor",
                    "metadata",
                    "tree",
                    "search",
                ],
            )]),
            // Git's own list: the top-level options `handle_options` in git.c gives the next
            // argument to. `--super-prefix` was removed in Git 2.41 and is still read by older
            // Gits; `--exec-path` takes a value only as `--exec-path=PATH` and is denied anyway.
            value_options: string_map(&[
                (
                    "git",
                    &[
                        "-C",
                        "-c",
                        "--git-dir",
                        "--work-tree",
                        "--namespace",
                        "--exec-path",
                        "--config-env",
                        "--super-prefix",
                        "--shallow-file",
                        "--attr-source",
                    ],
                ),
                ("gh", &["-R", "--repo"]),
                (
                    "cargo",
                    &[
                        "--manifest-path",
                        "--config",
                        "-Z",
                        "-p",
                        "--package",
                        "--target-dir",
                    ],
                ),
            ]),
            spool_dir: ".herdr-run".to_owned(),
            timeout_seconds: 900.0,
            retention_days: crate::retention::RETENTION_DAYS,
            max_panes: DEFAULT_MAX_PANES,
            ready_timeout_seconds: 0.0,
            readiness: "both".to_owned(),
            prompt_tail: None,
            shells: strings(&["bash", "zsh", "sh", "dash", "fish", "ksh"]),
            probe_remote: "https://github.com/git/git".to_owned(),
            broker: "direct".to_owned(),
            source_path: None,
            project_root: ".".to_owned(),
        }
    }
}

/// Every configuration key the parser accepts.
///
/// Public because it is also the definition of "every knob", which the generated
/// `.herdr-run.yaml` has to cover for the user guide to be able to point at that file instead of
/// restating it.
pub const KNOWN_KEYS: [&str; 21] = [
    "workspace",
    "tab_name",
    "cwd",
    "allow",
    "prefixes",
    "deny_global",
    "deny_subcommand",
    "deny_anywhere",
    "deny_subcommand_option",
    "allow_subcommand",
    "value_options",
    "spool_dir",
    "timeout_seconds",
    "retention_days",
    "max_panes",
    "ready_timeout_seconds",
    "readiness",
    "prompt_tail",
    "shells",
    "broker",
    "probe_remote",
];

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn string_map(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
    entries
        .iter()
        .map(|(key, values)| ((*key).to_owned(), strings(values)))
        .collect()
}

/// The built-in value-option table with `configured` added to it; nothing is taken away.
///
/// Each program keeps its built-in options first, in their built-in order, followed by the
/// configured ones it did not already have, in the order they were written; a program the table
/// does not know gets its configured options with repeats dropped.
fn with_builtin_value_options(
    configured: BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut merged = Config::default().value_options;
    for (program, options) in configured {
        let combined = merged.entry(program).or_default();
        for option in options {
            if !combined.contains(&option) {
                combined.push(option);
            }
        }
    }
    merged
}

/// Built-in deny entries that one configured list leaves out, and so does not enforce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingDefault {
    /// The list, as a dotted path: `deny_anywhere`, `deny_global.git`,
    /// `deny_subcommand_option.git.clone`.
    pub setting: String,
    /// The entries it leaves out, in built-in order.
    pub entries: Vec<String>,
}

/// Every built-in deny entry the configuration's deny lists leave out.
///
/// A configured deny list REPLACES the built-in one, so a list written before an entry was added
/// to the built-in defaults -- by an older `herdr-run init`, say -- goes on without it, and nothing
/// about the file says so. This names each such entry, so it can be said out loud.
///
/// Entries for a program the allowlist can never admit are not reported, because they could never
/// apply; nor are Cargo's [`CARGO_ALWAYS_DENIED`], which apply whatever the file says. Lists are
/// reported in [`KNOWN_KEYS`] order, programs and subcommands in sorted order, and entries in
/// built-in order.
#[must_use]
pub fn missing_defaults(config: &Config) -> Vec<MissingDefault> {
    let defaults = Config::default();
    let admissible = |program: &str| {
        config.allows_any_program() || config.allow.iter().any(|allowed| allowed == program)
    };
    let mut found = Vec::new();
    let mut compare =
        |setting: String, built_in: &[String], configured: &[String], exempt: &[&str]| {
            let absent: Vec<String> = built_in
                .iter()
                .filter(|entry| !configured.contains(entry) && !exempt.contains(&entry.as_str()))
                .cloned()
                .collect();
            if !absent.is_empty() {
                found.push(MissingDefault {
                    setting,
                    entries: absent,
                });
            }
        };
    for (program, built_in) in &defaults.deny_global {
        if admissible(program) {
            let exempt: &[&str] = if program == "cargo" {
                &CARGO_ALWAYS_DENIED
            } else {
                &[]
            };
            compare(
                format!("deny_global.{program}"),
                built_in,
                config
                    .deny_global
                    .get(program)
                    .map_or(&[][..], Vec::as_slice),
                exempt,
            );
        }
    }
    for (program, built_in) in &defaults.deny_subcommand {
        if admissible(program) {
            compare(
                format!("deny_subcommand.{program}"),
                built_in,
                config
                    .deny_subcommand
                    .get(program)
                    .map_or(&[][..], Vec::as_slice),
                &[],
            );
        }
    }
    compare(
        "deny_anywhere".to_owned(),
        &defaults.deny_anywhere,
        &config.deny_anywhere,
        &[],
    );
    for (program, subcommands) in &defaults.deny_subcommand_option {
        if !admissible(program) {
            continue;
        }
        let configured = config.deny_subcommand_option.get(program);
        for (subcommand, built_in) in subcommands {
            compare(
                format!("deny_subcommand_option.{program}.{subcommand}"),
                built_in,
                configured
                    .and_then(|entries| entries.get(subcommand))
                    .map_or(&[][..], Vec::as_slice),
                &[],
            );
        }
    }
    found
}

/// `deny_anywhere "--exec"; deny_global.git "-c", "--namespace"`: one line, every entry.
#[must_use]
pub fn describe_missing(missing: &[MissingDefault]) -> String {
    missing
        .iter()
        .map(|item| {
            format!(
                "{} {}",
                item.setting,
                item.entries
                    .iter()
                    .map(|entry| quote(entry))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The one warning line every subcommand that reads `config` prints, or `None`.
///
/// The line is the same every time for the same file, so it can be recognised. It names the file,
/// every left-out entry, and the remedy: `herdr-run init --upgrade` run where the file is, when the
/// file has a name `init` works on, and otherwise an edit by hand.
#[must_use]
pub fn missing_defaults_warning(config: &Config) -> Option<String> {
    let missing = missing_defaults(config);
    if missing.is_empty() {
        return None;
    }
    let source = config.source_path.as_deref();
    let shown = source.map_or_else(|| "<config>".to_owned(), display);
    let named_for_init = source
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| CONFIG_FILENAMES.contains(&name));
    let remedy = if named_for_init {
        format!(
            "run 'herdr-run init --upgrade' in {} or add them by hand",
            display(&config.project_root)
        )
    } else {
        "add them to the file by hand".to_owned()
    };
    Some(format!(
        "herdr-run: WARNING: {shown} leaves out built-in deny entries, so they are not enforced: {}. To adopt them, {remedy}.",
        describe_missing(&missing)
    ))
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                HerdrRunError::config(format!(
                    "cannot determine current directory: {}",
                    os_error_text(&error)
                ))
            })?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.parent().is_some() {
                    normalized.pop();
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// Search `start` and its ancestors for the nearest accepted config filename.
#[must_use]
pub fn find_config_file(start: &Path) -> Option<PathBuf> {
    let mut current = absolute_lexical(start).ok()?;
    loop {
        for filename in CONFIG_FILENAMES {
            let candidate = current.join(filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Load an explicit configuration file, the nearest discovered file, or built-in defaults.
pub fn load_config(explicit_path: Option<&Path>, start_dir: &Path) -> Result<Config> {
    let start = absolute_lexical(start_dir)?;
    let Some(start_text) = start.to_str() else {
        // Paths derived from this directory name the spool, the audit log, and the session cache.
        // Converting it lossily would retarget those writes into a different directory.
        return Err(HerdrRunError::config(format!(
            "current directory is not valid UTF-8: {}",
            quote(&start.to_string_lossy())
        )));
    };
    let path = if let Some(explicit) = explicit_path {
        if !explicit.is_file() {
            return Err(HerdrRunError::config(format!(
                "config file not found: {}",
                display(&explicit.to_string_lossy())
            )));
        }
        Some(absolute_lexical(explicit)?)
    } else {
        find_config_file(&start)
    };

    let Some(path) = path else {
        return Ok(Config {
            project_root: start_text.to_owned(),
            ..Config::default()
        });
    };
    let text = read_config_file(&path)?;
    let root = path.parent().unwrap_or_else(|| Path::new("/"));
    let root = if root.as_os_str().is_empty() {
        Path::new("/")
    } else {
        root
    };
    parse_config_text(
        &text,
        path.to_string_lossy().into_owned(),
        root.to_string_lossy().into_owned(),
    )
}

/// The text of the configuration file at `path`, which must be UTF-8.
pub fn read_config_file(path: &Path) -> Result<String> {
    let shown = display(&path.to_string_lossy());
    let payload = fs::read(path).map_err(|error| {
        HerdrRunError::config(format!(
            "cannot read config file {shown}: {}",
            os_error_text(&error)
        ))
    })?;
    String::from_utf8(payload).map_err(|_| {
        HerdrRunError::config(format!(
            "cannot read config file {shown}: the file is not valid UTF-8"
        ))
    })
}

/// Read configuration text as the file `source_path` holds it, and validate it.
pub fn parse_config_text(text: &str, source_path: String, project_root: String) -> Result<Config> {
    let document = core_load(text).map_err(|error| {
        HerdrRunError::config(format!("{}: invalid YAML: {error}", display(&source_path)))
    })?;
    parse_config(&document, Some(source_path), project_root)
}

fn require_object<'a>(value: &'a Yaml, what: &str) -> Result<&'a [(String, Yaml)]> {
    match value {
        Yaml::Map(entries) => Ok(entries),
        other => Err(HerdrRunError::config(format!(
            "{what}: expected an object, got {}",
            other.type_name()
        ))),
    }
}

fn require_array<'a>(value: &'a Yaml, what: &str) -> Result<&'a [Yaml]> {
    match value {
        Yaml::Seq(items) => Ok(items),
        other => Err(HerdrRunError::config(format!(
            "{what}: expected an array, got {}",
            other.type_name()
        ))),
    }
}

fn contains_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(u32::from(character), 0x00..=0x1f | 0x7f..=0x9f))
}

fn reject_controls(value: &str, what: &str) -> Result<()> {
    if contains_control(value) {
        return Err(HerdrRunError::config(format!(
            "{what}: control characters and Unicode surrogates are not allowed"
        )));
    }
    Ok(())
}

fn require_text(value: &Yaml, what: &str) -> Result<String> {
    let Yaml::Str(text) = value else {
        return Err(HerdrRunError::config(format!(
            "{what}: must be a string, got {}",
            value.type_name()
        )));
    };
    reject_controls(text, what)?;
    Ok(text.clone())
}

/// A name the policy compares with typed text: a program, a wrapper, an option, a subcommand, a
/// workspace, a tab schema, or a shell.
///
/// Besides control characters it may hold no invisible character -- a format character such as a
/// zero-width space, a byte-order mark, or a bidirectional override, or a non-ASCII space such as
/// U+00A0 or U+2028 -- because a name holding one looks exactly like the name without it, and a
/// deny entry that looks like `-c` but is not would never match the `-c` it appears to deny.
/// Nor may it hold a noncharacter, a private-use character, or a code point Unicode 16.0 leaves
/// unassigned, which draw as whatever the font in use makes of them, often nothing
/// ([`name_violation`]).
fn require_policy_name(value: &Yaml, what: &str) -> Result<String> {
    let text = require_text(value, what)?;
    if text.is_empty() {
        return Err(HerdrRunError::config(format!("{what}: must not be empty")));
    }
    if let Some(violation) = name_violation(&text) {
        return Err(HerdrRunError::config(format!(
            "{what}: {} contains {violation}; a name may not contain one, because it would look like a name it is not",
            quote(&text)
        )));
    }
    Ok(text)
}

fn require_optional_text(value: &Yaml, what: &str) -> Result<Option<String>> {
    if matches!(value, Yaml::Null) {
        Ok(None)
    } else {
        require_text(value, what).map(Some)
    }
}

fn require_policy_list(value: &Yaml, what: &str) -> Result<Vec<String>> {
    let items = require_array(value, what)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if !matches!(item, Yaml::Str(_)) {
            return Err(HerdrRunError::config(format!(
                "{what}: every entry must be a string, got {}",
                item.type_name()
            )));
        }
        out.push(require_policy_name(
            item,
            &format!("{what}[{}]", out.len()),
        )?);
    }
    Ok(out)
}

fn require_policy_map(value: &Yaml, what: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let entries = require_object(value, what)?;
    let mut out = BTreeMap::new();
    for (key, item) in entries {
        require_policy_name(&Yaml::Str(key.clone()), &format!("{what} key"))?;
        out.insert(
            key.clone(),
            require_policy_list(item, &format!("{what}.{key}"))?,
        );
    }
    Ok(out)
}

fn require_policy_map_map(
    value: &Yaml,
    what: &str,
) -> Result<BTreeMap<String, BTreeMap<String, Vec<String>>>> {
    let entries = require_object(value, what)?;
    let mut out = BTreeMap::new();
    for (key, item) in entries {
        require_policy_name(&Yaml::Str(key.clone()), &format!("{what} key"))?;
        out.insert(
            key.clone(),
            require_policy_map(item, &format!("{what}.{key}"))?,
        );
    }
    Ok(out)
}

fn require_seconds(value: &Yaml, what: &str) -> Result<f64> {
    let number = match value {
        Yaml::Int(integer) => {
            if *integer < 0 {
                return Err(HerdrRunError::config(format!(
                    "{what}: must not be negative"
                )));
            }
            if *integer > MAX_TIMEOUT_SECONDS as i128 {
                return Err(HerdrRunError::config(format!(
                    "{what}: must not exceed {MAX_TIMEOUT_SECONDS:.0} seconds"
                )));
            }
            return Ok(*integer as f64);
        }
        Yaml::Float(number) => *number,
        other => {
            return Err(HerdrRunError::config(format!(
                "{what}: must be a number, got {}",
                other.type_name()
            )))
        }
    };
    if !number.is_finite() {
        return Err(HerdrRunError::config(format!("{what}: must be finite")));
    }
    if number < 0.0 {
        return Err(HerdrRunError::config(format!(
            "{what}: must not be negative"
        )));
    }
    if number > MAX_TIMEOUT_SECONDS {
        return Err(HerdrRunError::config(format!(
            "{what}: must not exceed {MAX_TIMEOUT_SECONDS:.0} seconds"
        )));
    }
    if !whole_milliseconds(number) {
        return Err(HerdrRunError::config(format!(
            "{what}: must be a whole number of milliseconds"
        )));
    }
    Ok(number)
}

fn require_bounded_count(value: &Yaml, what: &str, limit: u64, unit: &str) -> Result<u64> {
    let Yaml::Int(integer) = value else {
        return Err(HerdrRunError::config(format!(
            "{what}: must be a non-negative integer, got {}",
            value.type_name()
        )));
    };
    if *integer < 0 {
        return Err(HerdrRunError::config(format!(
            "{what}: must be a non-negative integer, got number"
        )));
    }
    if *integer > i128::from(limit) {
        return Err(HerdrRunError::config(format!(
            "{what}: must not exceed {limit}{unit}"
        )));
    }
    Ok(u64::try_from(*integer).unwrap_or(limit))
}

fn require_choice(value: &Yaml, what: &str, allowed: &[&str]) -> Result<String> {
    let choice = require_text(value, what)?;
    if allowed.contains(&choice.as_str()) {
        Ok(choice)
    } else {
        Err(HerdrRunError::config(format!(
            "{what}: must be one of {}; got {}",
            allowed.join(", "),
            quote(&choice)
        )))
    }
}

enum TemplateProblem {
    Malformed(&'static str),
    Placeholders,
}

/// Describe what is wrong with a tab-name schema, or return `None` when it is valid.
fn template_problem(schema: &str) -> Option<TemplateProblem> {
    let characters: Vec<char> = schema.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '{' => {
                if characters.get(index + 1) == Some(&'{') {
                    index += 2;
                    continue;
                }
                let Some(offset) = characters[index + 1..].iter().position(|c| *c == '}') else {
                    return Some(TemplateProblem::Malformed("unmatched '{'"));
                };
                let close = index + 1 + offset;
                let field: String = characters[index + 1..close].iter().collect();
                if field != "agent" && field != "project" {
                    return Some(TemplateProblem::Placeholders);
                }
                index = close + 1;
            }
            '}' => {
                if characters.get(index + 1) == Some(&'}') {
                    index += 2;
                    continue;
                }
                return Some(TemplateProblem::Malformed("unmatched '}'"));
            }
            _ => index += 1,
        }
    }
    None
}

fn check_template(schema: &str, what: &str) -> Result<()> {
    match template_problem(schema) {
        None => Ok(()),
        Some(TemplateProblem::Placeholders) => Err(HerdrRunError::config(format!(
            "{what}: schema {} may use only plain {{agent}} and {{project}} placeholders",
            quote(schema)
        ))),
        Some(TemplateProblem::Malformed(detail)) => Err(HerdrRunError::config(format!(
            "{what}: schema {} is malformed: {detail}",
            quote(schema)
        ))),
    }
}

/// Render a tab-label schema restricted to literal text and plain `{agent}`/`{project}` fields.
pub fn render_tab_name(schema: &str, agent: &str, project: &str) -> Result<String> {
    check_template(schema, "tab_name")?;
    let characters: Vec<char> = schema.chars().collect();
    let mut output = String::new();
    let mut index = 0;
    while index < characters.len() {
        let rest = &characters[index..];
        if rest.starts_with(&['{', '{']) {
            output.push('{');
            index += 2;
        } else if rest.starts_with(&['}', '}']) {
            output.push('}');
            index += 2;
        } else if rest[0] == '{' {
            let offset = rest.iter().position(|c| *c == '}').unwrap_or(rest.len());
            let field: String = rest[1..offset].iter().collect();
            output.push_str(if field == "agent" { agent } else { project });
            index += offset + 1;
        } else {
            output.push(rest[0]);
            index += 1;
        }
    }
    reject_controls(&output, "rendered tab_name")?;
    if output.is_empty() {
        return Err(HerdrRunError::config(
            "rendered tab_name: must not be empty",
        ));
    }
    Ok(output)
}

/// Validate an already decoded document and construct a resolved configuration.
pub fn parse_config(
    document: &Yaml,
    source_path: Option<String>,
    project_root: String,
) -> Result<Config> {
    let mut config = Config {
        source_path: source_path.clone(),
        project_root,
        ..Config::default()
    };
    if matches!(document, Yaml::Null) {
        return Ok(config);
    }
    // The path heads every message below; `display` keeps a control character in a directory name
    // from reaching the terminal raw.
    let shown = source_path
        .as_deref()
        .map_or_else(|| "<config>".to_owned(), display);
    let what = shown.as_str();
    let mapping = require_object(document, what)?;
    let mut unknown: Vec<&str> = mapping
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !KNOWN_KEYS.contains(key))
        .collect();
    unknown.sort_unstable();
    unknown.dedup();
    if !unknown.is_empty() {
        let mut known = KNOWN_KEYS.to_vec();
        known.sort_unstable();
        // Reject rather than ignore: a typo'd `allowlist:` key silently falling back to the
        // default allowlist is exactly the kind of quiet policy failure this tool must not have.
        return Err(HerdrRunError::config(format!(
            "{what}: unknown key(s): {}. Known keys: {}",
            unknown
                .iter()
                .map(|key| quote(key))
                .collect::<Vec<_>>()
                .join(", "),
            known.join(", ")
        )));
    }
    for key in KNOWN_KEYS {
        let Some((_, value)) = mapping.iter().find(|(name, _)| name == key) else {
            continue;
        };
        let where_ = format!("{what}.{key}");
        match key {
            "workspace" => config.workspace = require_policy_name(value, &where_)?,
            "tab_name" => {
                let schema = require_policy_name(value, &where_)?;
                check_template(&schema, &where_)?;
                config.tab_name = schema;
            }
            "cwd" => config.cwd = require_optional_text(value, &where_)?,
            "allow" => config.allow = require_policy_list(value, &where_)?,
            "prefixes" => config.prefixes = require_policy_list(value, &where_)?,
            "deny_global" => config.deny_global = require_policy_map(value, &where_)?,
            "deny_subcommand" => config.deny_subcommand = require_policy_map(value, &where_)?,
            "deny_anywhere" => config.deny_anywhere = require_policy_list(value, &where_)?,
            "deny_subcommand_option" => {
                config.deny_subcommand_option = require_policy_map_map(value, &where_)?;
            }
            "allow_subcommand" => config.allow_subcommand = require_policy_map(value, &where_)?,
            "value_options" => {
                config.value_options =
                    with_builtin_value_options(require_policy_map(value, &where_)?);
            }
            "spool_dir" => config.spool_dir = require_text(value, &where_)?,
            "timeout_seconds" => config.timeout_seconds = require_seconds(value, &where_)?,
            "retention_days" => {
                config.retention_days = require_bounded_count(
                    value,
                    &where_,
                    crate::retention::MAX_RETENTION_DAYS,
                    " days",
                )?;
            }
            "max_panes" => {
                config.max_panes = require_bounded_count(value, &where_, MAX_PANE_CAP, "")?;
            }
            "ready_timeout_seconds" => {
                config.ready_timeout_seconds = require_seconds(value, &where_)?;
            }
            "readiness" => {
                config.readiness = require_choice(value, &where_, &["both", "process"])?;
            }
            "prompt_tail" => config.prompt_tail = require_optional_text(value, &where_)?,
            "shells" => config.shells = require_policy_list(value, &where_)?,
            "broker" => {
                config.broker = require_choice(value, &where_, &["direct", "systemd-run"])?;
            }
            "probe_remote" => config.probe_remote = require_text(value, &where_)?,
            _ => {}
        }
    }
    if config.allow.is_empty() {
        return Err(HerdrRunError::config(format!(
            "{what}.allow: refusing an EMPTY allowlist — no command could ever run"
        )));
    }
    if config.allows_any_program() && config.allow.len() > 1 {
        return Err(HerdrRunError::config(format!(
            "{what}.allow: \"{ALLOW_ANY_PROGRAM}\" already admits every program, so it must be the only entry; listing programs beside it makes the policy look narrower than it is"
        )));
    }
    if config.allow.iter().any(|program| program == "cargo")
        && !config.allow_subcommand.contains_key("cargo")
    {
        return Err(HerdrRunError::config(format!(
            "{what}.allow_subcommand: cargo is allowed but has no positive subcommand list"
        )));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-run-config-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        path
    }

    fn parse_text(text: &str) -> Result<Config> {
        let document = core_load(text).map_err(|error| HerdrRunError::config(error.to_string()))?;
        parse_config(&document, Some("x.yaml".into()), "/tmp".into())
    }

    #[test]
    fn defaults_match_the_python_policy() {
        let config = Config::default();
        assert_eq!(config.workspace, "agent-cmds");
        assert_eq!(config.tab_name, "{agent}");
        assert_eq!(config.allow, strings(&["git", "gh"]));
        assert_eq!(
            config.allow_subcommand.get("cargo").unwrap(),
            &strings(&[
                "fetch",
                "update",
                "generate-lockfile",
                "vendor",
                "metadata",
                "tree",
                "search",
            ])
        );
        assert_eq!(config.prefixes, strings(&["with-proxy"]));
        assert_eq!(config.timeout_seconds, 900.0);
        assert_eq!(config.retention_days, 4);
        // Named literally rather than compared against DEFAULT_MAX_PANES: a test that reads the
        // production constant would agree with any value the constant ever takes, including a
        // typo, and so would pin nothing at all.
        assert_eq!(config.max_panes, 32);
        assert_eq!(config.readiness, "both");
        assert_eq!(config.broker, "direct");
    }

    /// Every ``  `key` (value by default)  `` the shipped guide states in prose.
    fn prose_defaults(guide: &str) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for (end, _) in guide.match_indices(" by default)") {
            let head = &guide[..end];
            let Some(open) = head.rfind('(') else {
                continue;
            };
            let value = &head[open + 1..];
            if value.is_empty() || value.contains(')') {
                continue;
            }
            let before = head[..open].trim_end();
            let Some(stripped) = before.strip_suffix('`') else {
                continue;
            };
            let Some(tick) = stripped.rfind('`') else {
                continue;
            };
            let key = &stripped[tick + 1..];
            if key.is_empty()
                || !key
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_')
            {
                continue;
            }
            found.push((key.to_owned(), value.to_owned()));
        }
        found
    }

    /// The guide restates two defaults in prose; those sentences must say what the code does.
    #[test]
    fn user_guide_prose_defaults_are_the_built_in_defaults() {
        let expected: &[(&str, &str, u64)] =
            &[("retention_days", "four", 4), ("max_panes", "32", 32)];
        let config = Config::default();
        for (key, _spelling, number) in expected {
            let actual = match *key {
                "retention_days" => config.retention_days,
                "max_panes" => config.max_panes,
                other => panic!("nothing reads the default for {other}"),
            };
            assert_eq!(
                actual, *number,
                "the built-in {key} is not what the guide says"
            );
        }

        let found = prose_defaults(crate::USER_GUIDE);
        assert!(
            !found.is_empty(),
            "the user guide no longer restates any default in prose; drop the entries here too"
        );
        for (key, spelling) in &found {
            let pinned = expected
                .iter()
                .find(|(name, _, _)| name == key)
                .unwrap_or_else(|| {
                    panic!("the guide restates a default for '{key}' that nothing pins")
                });
            assert_eq!(
                spelling, pinned.1,
                "the user guide says {key} defaults to '{spelling}'"
            );
        }
        for (key, _spelling, _number) in expected {
            assert!(
                found.iter().any(|(name, _)| name == key),
                "a pinned prose default is no longer in the shipped guide: {key}"
            );
        }
    }

    #[test]
    fn parses_every_supported_key_and_replaces_maps_wholesale() {
        let config = parse_text(
            "workspace: cmds\ntab_name: '{project}-{agent}'\ncwd: sub\nallow: [cargo]\n\
             prefixes: []\ndeny_global: {cargo: [-Z]}\ndeny_subcommand: {cargo: [install]}\n\
             deny_anywhere: [--evil]\nallow_subcommand: {cargo: [fetch, build]}\n\
             value_options: {cargo: [--manifest-path]}\nspool_dir: out\ntimeout_seconds: 30\n\
             retention_days: 7\nready_timeout_seconds: 5\nreadiness: process\n\
             prompt_tail: '% '\nshells: [zsh]\nbroker: systemd-run\n\
             probe_remote: https://example.test/repo\n",
        )
        .unwrap();
        assert_eq!(config.allow, strings(&["cargo"]));
        assert_eq!(config.deny_global.len(), 1);
        assert!(!config.deny_global.contains_key("git"));
        assert_eq!(
            config.allow_subcommand["cargo"],
            strings(&["fetch", "build"])
        );
        assert_eq!(config.timeout_seconds, 30.0);
        assert_eq!(config.retention_days, 7);
        assert_eq!(config.prompt_tail.as_deref(), Some("% "));
    }

    #[test]
    fn every_malformed_shape_is_a_typed_config_error() {
        for text in [
            "allow: git\n",
            "allow: [1]\n",
            "workspace: 42\n",
            "timeout_seconds: true\n",
            "timeout_seconds: -1\n",
            "timeout_seconds: 31536001\n",
            "timeout_seconds: 0.0001\n",
            "retention_days: -1\n",
            "retention_days: 1.5\n",
            "retention_days: '4'\n",
            "retention_days: 365001\n",
            "readiness: maybe\n",
            "deny_global: [git]\n",
            "allow: []\n",
            "allowlist: [git]\n",
        ] {
            let error = parse_text(text).expect_err(text);
            assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG, "{text}");
        }
    }

    #[test]
    fn the_allow_wildcard_must_stand_alone() {
        let error = parse_text("allow: ['*', git]\n").expect_err("a wildcard mixed with names");
        assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
        assert!(
            error.message().contains("must be the only entry"),
            "{error}"
        );
        let wildcard = parse_text("allow: ['*']\n").expect("a lone wildcard is valid");
        assert!(wildcard.allows_any_program());
        assert!(!Config::default().allows_any_program());
    }

    #[test]
    fn cargo_allow_requires_its_positive_subcommand_map() {
        let error = parse_text("allow: [git, cargo]\nallow_subcommand: {custom-tool: [inspect]}\n")
            .unwrap_err();
        assert!(error
            .message()
            .contains("cargo is allowed but has no positive"));
    }

    #[test]
    fn nearest_config_wins_and_yaml_name_precedes_yml() {
        let root = temporary_directory("nearest");
        let nested = root.join("slot/deep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(".herdr-run.yaml"), "workspace: outer\n").unwrap();
        fs::write(root.join(".herdr-run.yml"), "workspace: wrong-name\n").unwrap();
        fs::write(root.join("slot/.herdr-run.yaml"), "workspace: inner\n").unwrap();
        let config = load_config(None, &nested).unwrap();
        assert_eq!(config.workspace, "inner");
        assert_eq!(config.project_root, root.join("slot").to_string_lossy());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_missing_and_invalid_yaml_are_code_78_errors() {
        let root = temporary_directory("invalid");
        let missing = root.join("missing.yaml");
        assert_eq!(
            load_config(Some(&missing), &root).unwrap_err().exit_code(),
            crate::error::EXIT_CONFIG
        );
        let invalid = root.join("invalid.yaml");
        fs::write(&invalid, "allow: [unterminated\n").unwrap();
        let error = load_config(Some(&invalid), &root).unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
        assert!(
            error
                .message()
                .ends_with("invalid YAML: unterminated flow sequence at line 1 column 8"),
            "{error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plain_scalars_resolve_by_the_documented_rule() {
        let config =
            parse_text("workspace: yes\nallow: [git, on, off]\ntimeout_seconds: 0o10\n").unwrap();
        assert_eq!(config.workspace, "yes");
        assert_eq!(config.allow, strings(&["git", "on", "off"]));
        assert_eq!(config.timeout_seconds, 8.0);
    }

    #[test]
    fn templates_empty_policy_names_and_controls_are_rejected() {
        for text in [
            "tab_name: '{agent.name}'\n",
            "tab_name: '{agent!r}'\n",
            "tab_name: '{agent:>10}'\n",
            "tab_name: '{unknown}'\n",
            "tab_name: 'x}'\n",
            "tab_name: 'x{'\n",
            "allow: ['']\n",
            "prefixes: [\"with\\eproxy\"]\n",
            "deny_global: {'': [-c]}\n",
            "shells: [\"ba\\Nsh\"]\n",
        ] {
            let error = parse_text(text).expect_err(text);
            assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG, "{text}");
        }
        for valid in [
            "literal",
            "{agent}",
            "{project}-{agent}",
            "x{agent}y{project}z",
            "{{literal}}-{agent}",
            "{{{agent}}}",
        ] {
            let config = parse_text(&format!("tab_name: '{valid}'\n")).unwrap();
            assert_eq!(config.tab_name, valid);
        }
        assert_eq!(
            render_tab_name("{{{agent}}}-{project}", "release", "repo").unwrap(),
            "{release}-repo"
        );
        assert!(render_tab_name("{agent}", "bad\nagent", "repo").is_err());
    }

    #[test]
    fn empty_yaml_uses_defaults_but_records_its_source() {
        let root = temporary_directory("empty");
        let path = root.join(".herdr-run.yaml");
        fs::write(&path, "").unwrap();
        let config = load_config(None, &root).unwrap();
        assert_eq!(config.allow, strings(&["git", "gh"]));
        let expected_source = path.to_string_lossy().into_owned();
        assert_eq!(
            config.source_path.as_deref(),
            Some(expected_source.as_str())
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The reported repro, byte for byte: a deny entry ending in U+2028, which YAML 1.1 reads as a
    /// line break and YAML 1.2 does not. It was accepted and never matched, so `git -c ...` ran.
    #[test]
    fn a_raw_yaml_1_1_line_break_is_refused() {
        let root = temporary_directory("line-separator");
        let path = root.join(".herdr-run.yaml");
        for document in [
            &b"allow: [git]\ndeny_global:\n  git: [-c\xe2\x80\xa8, --exec-path\xe2\x80\xa9]\n"[..],
            &b"deny_anywhere: [--upload-pack\xe2\x80\xa8]\n"[..],
            &b"value_options:\n  git: [-C\xe2\x80\xa9]\n"[..],
            &b"allow: [git]\n# a comment\xc2\x85allow: [curl]\n"[..],
        ] {
            fs::write(&path, document).unwrap();
            let error = load_config(Some(&path), &root).expect_err("must be refused");
            assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
            assert!(
                error
                    .message()
                    .contains("YAML 1.1 reads it as a line break"),
                "{error}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn parse_at(text: &str, source: &str) -> Config {
        let document = core_load(text).expect("valid YAML");
        let root = Path::new(source)
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        parse_config(&document, Some(source.to_owned()), root).expect("valid configuration")
    }

    /// Which options take the next argument is how git reads its own command line, not policy.
    ///
    /// A file written by an older `herdr-run init` lists the seven git options that release knew.
    /// It used to REPLACE the built-in table, so `git --attr-source HEAD daemon` read `HEAD` as the
    /// subcommand and ran a denied one.
    #[test]
    fn value_options_add_to_the_built_in_table_and_never_take_from_it() {
        let built_in = Config::default().value_options;
        let older = parse_text(
            "value_options:\n  git: [-C, -c, --git-dir, --work-tree, --namespace, --exec-path, --config-env]\n",
        )
        .unwrap();
        assert_eq!(older.value_options, built_in);
        assert_eq!(
            parse_text("value_options: {}\n").unwrap().value_options,
            built_in
        );
        assert_eq!(
            parse_text("value_options: {gh: [], cargo: []}\n")
                .unwrap()
                .value_options,
            built_in
        );

        let added =
            parse_text("value_options:\n  git: [--frobnicate, -C, --frobnicate]\n").unwrap();
        let mut expected = built_in["git"].clone();
        expected.push("--frobnicate".to_owned());
        assert_eq!(added.value_options["git"], expected);
        assert_eq!(added.value_options["gh"], built_in["gh"]);
        let tool = parse_text("allow: [tool]\nvalue_options:\n  tool: [-f, -f, -g]\n").unwrap();
        assert_eq!(tool.value_options["tool"], strings(&["-f", "-g"]));
        assert_eq!(tool.value_options["git"], built_in["git"]);
    }

    fn missing(setting: &str, entries: &[&str]) -> MissingDefault {
        MissingDefault {
            setting: setting.to_owned(),
            entries: strings(entries),
        }
    }

    #[test]
    fn a_deny_list_that_leaves_out_a_built_in_entry_is_reported() {
        let older = parse_text("deny_anywhere: [--upload-pack, --receive-pack]\n").unwrap();
        assert_eq!(
            missing_defaults(&older),
            vec![missing("deny_anywhere", &["--exec"])]
        );
        assert!(missing_defaults(&Config::default()).is_empty());
        let wider =
            parse_text("deny_anywhere: [--upload-pack, --receive-pack, --exec, --evil]\n").unwrap();
        assert!(missing_defaults(&wider).is_empty());

        let every = parse_text(
            "allow: ['*']\ndeny_global: {}\ndeny_subcommand: {git: [daemon]}\ndeny_anywhere: []\n\
             deny_subcommand_option: {git: {clone: []}}\n",
        )
        .unwrap();
        assert_eq!(
            missing_defaults(&every),
            vec![
                // Cargo's --config and -Z apply whatever the file says, so they are never left out.
                missing(
                    "deny_global.git",
                    &["-c", "--config-env", "--exec-path", "--namespace"]
                ),
                missing(
                    "deny_subcommand.gh",
                    &["alias", "extension", "ext", "codespace", "cs"]
                ),
                missing("deny_subcommand.git", &["filter-branch", "instaweb"]),
                missing(
                    "deny_anywhere",
                    &["--upload-pack", "--receive-pack", "--exec"]
                ),
                missing("deny_subcommand_option.git.clone", &["-u"]),
                missing("deny_subcommand_option.git.difftool", &["-x", "--extcmd"]),
                missing("deny_subcommand_option.git.rebase", &["-x"]),
            ]
        );
    }

    #[test]
    fn entries_for_a_program_that_can_never_run_are_not_reported() {
        let mut narrow = parse_text(
            "allow: [git]\ndeny_global: {git: [-c, --config-env, --exec-path, --namespace]}\n\
             deny_subcommand: {git: [filter-branch, daemon, instaweb]}\n",
        )
        .unwrap();
        assert!(missing_defaults(&narrow).is_empty());
        narrow.allow = strings(&["git", "gh"]);
        assert_eq!(
            missing_defaults(&narrow),
            vec![missing(
                "deny_subcommand.gh",
                &["alias", "extension", "ext", "codespace", "cs"]
            )]
        );
    }

    #[test]
    fn the_warning_names_the_file_every_entry_and_the_remedy() {
        let older = parse_at("deny_anywhere: [--upload-pack]\n", "/p/.herdr-run.yaml");
        assert_eq!(
            missing_defaults_warning(&older).as_deref(),
            Some(
                "herdr-run: WARNING: /p/.herdr-run.yaml leaves out built-in deny entries, so they are not enforced: deny_anywhere \"--receive-pack\", \"--exec\". To adopt them, run 'herdr-run init --upgrade' in /p or add them by hand."
            )
        );
        let yml = parse_at("deny_anywhere: [--upload-pack]\n", "/p/.herdr-run.yml");
        assert!(missing_defaults_warning(&yml)
            .unwrap()
            .contains("run 'herdr-run init --upgrade' in /p or"));
        // `init --upgrade` works only on the two names `init` writes, so it is not offered for
        // another.
        let other = parse_at("deny_anywhere: [--upload-pack]\n", "/p/conf/settings.yaml");
        assert!(missing_defaults_warning(&other)
            .unwrap()
            .ends_with("To adopt them, add them to the file by hand."));
        assert_eq!(missing_defaults_warning(&Config::default()), None);
        assert_eq!(
            missing_defaults_warning(&parse_at("workspace: x\n", "/p/.herdr-run.yaml")),
            None
        );
    }

    #[test]
    fn an_invisible_character_in_a_policy_name_is_refused() {
        for (document, place) in [
            (
                "allow: [git]\ndeny_global:\n  git: [\"-c\\L\", --exec-path]\n",
                "deny_global.git[0]",
            ),
            (
                "deny_anywhere: [\"--upload-pack\\u200b\"]\n",
                "deny_anywhere[0]",
            ),
            (
                "value_options:\n  git: [\"-C\\ufeff\"]\n",
                "value_options.git[0]",
            ),
            ("allow: [\"git\\u00a0\"]\n", "allow[0]"),
            ("prefixes: [\"with-proxy\\u202e\"]\n", "prefixes[0]"),
            ("workspace: \"agent\\u2060cmds\"\n", "workspace"),
            (
                "deny_subcommand:\n  \"git\\u3000\": [daemon]\n",
                "deny_subcommand key",
            ),
        ] {
            let error = parse_text(document).expect_err(document);
            let message = error.message();
            assert!(message.contains(&format!(".{place}: ")), "{message}");
            assert!(message.contains("invisible character U+"), "{message}");
        }
        let config = parse_text(
            "workspace: café\nprompt_tail: \"\\u00a0$ \"\nspool_dir: \"out\\u00a0dir\"\n",
        )
        .unwrap();
        assert_eq!(config.workspace, "café");
        assert_eq!(config.prompt_tail.as_deref(), Some("\u{a0}$ "));
        assert_eq!(config.spool_dir, "out\u{a0}dir");
    }

    /// A noncharacter, a private-use character, or an unassigned code point draws as whatever the
    /// font makes of it, often nothing; U+1D159 and U+16FE4 are assigned but draw nothing at all.
    #[test]
    fn a_character_with_no_settled_appearance_in_a_policy_name_is_refused() {
        for (document, place, what) in [
            (
                "allow: [git, \"git\\ufdd0\"]\n",
                "allow[1]",
                "noncharacter U+FDD0",
            ),
            (
                "deny_anywhere: [\"--upload-pack\\U0010FFFF\"]\n",
                "deny_anywhere[0]",
                "noncharacter U+10FFFF",
            ),
            (
                "prefixes: [\"with-proxy\\ue000\"]\n",
                "prefixes[0]",
                "private-use character U+E000",
            ),
            (
                "workspace: \"w\\U000F0000\"\n",
                "workspace",
                "private-use character U+F0000",
            ),
            (
                "allow: [git]\ndeny_global:\n  git: [\"-c\\u0378\", --exec-path]\n",
                "deny_global.git[0]",
                "unassigned code point U+0378",
            ),
            (
                "value_options:\n  git: [\"-C\\U000E1000\"]\n",
                "value_options.git[0]",
                "unassigned code point U+E1000",
            ),
            (
                "tab_name: \"{agent}\\U0001D159\"\n",
                "tab_name",
                "invisible character U+1D159",
            ),
            (
                "deny_subcommand:\n  git: [\"daemon\\U00016FE4\"]\n",
                "deny_subcommand.git[0]",
                "invisible character U+16FE4",
            ),
        ] {
            let error = parse_text(document).expect_err(document);
            let message = error.message();
            assert!(message.contains(&format!(".{place}: ")), "{message}");
            assert!(
                message.contains(&format!(" contains {what}; ")),
                "{message}"
            );
        }
    }

    #[test]
    fn a_letter_unicode_16_assigned_is_a_name_and_odd_text_outside_names_is_accepted() {
        let config = parse_text(
            "workspace: \"\\u1C89\"\nprompt_tail: \"\\ue000$ \"\nspool_dir: \"out\\u0378\"\n",
        )
        .unwrap();
        assert_eq!(config.workspace, "\u{1c89}");
        assert_eq!(config.prompt_tail.as_deref(), Some("\u{e000}$ "));
        assert_eq!(config.spool_dir, "out\u{378}");
    }
}
