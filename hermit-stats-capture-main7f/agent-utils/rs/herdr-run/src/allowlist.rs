//! Command admission and safe shell rendering.
//!
//! A leading `~` in an argument is expanded BEFORE the policy looks at it, so the policy judges the
//! argument exactly as it will run: `git ~ foo` with `HOME=--exec-path=/tmp/evil` is judged as
//! `git --exec-path=/tmp/evil foo` and refused, rather than admitted as a harmless-looking path and
//! then executed as an option. The wrapper prefixes and the program are never expanded; a program
//! spelled with a leading `~` is refused outright.

use std::ffi::{CStr, CString, OsStr, OsString};

use crate::config::{Config, CARGO_ALWAYS_DENIED};
use crate::error::{HerdrRunError, Result};
use crate::textfmt::quote;

/// An admitted command together with the policy facts used to admit it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Admission {
    /// Full argv as split, including any allowed wrapper prefixes, before `~` expansion.
    pub argv: Vec<String>,
    /// Allowed wrapper chain preceding the program.
    pub prefix: Vec<String>,
    /// The allowlisted bare program name.
    pub program: String,
    /// First non-option program argument, when present.
    pub subcommand: Option<String>,
    rendered: String,
}

impl Admission {
    /// Render this admission as an exact, injection-safe shell word list.
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitState {
    Unquoted,
    SingleQuoted,
    DoubleQuoted,
    EscapedUnquoted,
    EscapedDouble,
}

fn split(command: &str) -> Result<Vec<String>> {
    // This small state machine intentionally does not use shell_words::split: that parser enables
    // shell comments, while Python's shlex.split(..., comments=False, posix=True) treats `#` as an
    // ordinary argument character. It also preserves Python shlex's narrower double-quote escape
    // rule: only backslash and double quote lose the backslash inside double quotes.
    let mut state = SplitState::Unquoted;
    let mut token = String::new();
    let mut token_started = false;
    let mut output = Vec::new();
    for character in command.chars() {
        match state {
            SplitState::Unquoted => match character {
                ' ' | '\t' | '\r' | '\n' => {
                    if token_started {
                        output.push(std::mem::take(&mut token));
                        token_started = false;
                    }
                }
                '\'' => {
                    token_started = true;
                    state = SplitState::SingleQuoted;
                }
                '"' => {
                    token_started = true;
                    state = SplitState::DoubleQuoted;
                }
                '\\' => {
                    token_started = true;
                    state = SplitState::EscapedUnquoted;
                }
                other => {
                    token_started = true;
                    token.push(other);
                }
            },
            SplitState::SingleQuoted => {
                if character == '\'' {
                    state = SplitState::Unquoted;
                } else {
                    token.push(character);
                }
            }
            SplitState::DoubleQuoted => match character {
                '"' => state = SplitState::Unquoted,
                '\\' => state = SplitState::EscapedDouble,
                other => token.push(other),
            },
            SplitState::EscapedUnquoted => {
                token.push(character);
                state = SplitState::Unquoted;
            }
            SplitState::EscapedDouble => {
                if character != '\\' && character != '"' {
                    token.push('\\');
                }
                token.push(character);
                state = SplitState::DoubleQuoted;
            }
        }
    }
    match state {
        SplitState::SingleQuoted | SplitState::DoubleQuoted => {
            return Err(HerdrRunError::refused(
                "cannot parse command (unbalanced quoting?): No closing quotation",
            ));
        }
        SplitState::EscapedUnquoted | SplitState::EscapedDouble => {
            return Err(HerdrRunError::refused(
                "cannot parse command (unbalanced quoting?): No escaped character",
            ));
        }
        SplitState::Unquoted => {}
    }
    if token_started {
        output.push(token);
    }
    if output.is_empty() {
        return Err(HerdrRunError::refused("empty command"));
    }
    Ok(output)
}

fn first_terminal_control(value: &str) -> Option<u32> {
    value
        .chars()
        .map(u32::from)
        .find(|codepoint| matches!(codepoint, 0x00..=0x1f | 0x7f..=0x9f))
}

fn control_refusal(value: &str, what: &str) -> Option<HerdrRunError> {
    first_terminal_control(value).map(|codepoint| {
        HerdrRunError::refused(format!(
            "{what} contains terminal control U+{codepoint:04X}; control characters cannot be injected into a shared pane"
        ))
    })
}

/// Admit `command` under `config`, or return a typed refusal before any pane interaction.
///
/// Every argument after the program is `~`-expanded first, with `HOME` read from this process's
/// environment, and every rule judges the expanded argument, which is exactly what is rendered and
/// run.
pub fn admit(command: &str, config: &Config) -> Result<Admission> {
    admit_with_home(command, config, std::env::var_os("HOME").as_deref())
}

/// [`admit`] with an explicit value for `HOME` (`None` when it is unset).
pub fn admit_with_home(command: &str, config: &Config, home: Option<&OsStr>) -> Result<Admission> {
    if let Some(error) = control_refusal(command, "command") {
        return Err(error);
    }
    let argv = split(command)?;
    let mut rest = argv.as_slice();
    let mut prefix = Vec::new();
    while let Some(first) = rest.first() {
        if !config.prefixes.contains(first) {
            break;
        }
        if prefix.contains(first) {
            return Err(HerdrRunError::refused(format!(
                "prefix {} repeated; each wrapper may appear at most once",
                quote(first)
            )));
        }
        prefix.push(first.clone());
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return Err(HerdrRunError::refused(format!(
            "command is only wrapper prefixes ({}) with no program. Allowed programs: {}",
            prefix.join(" "),
            sorted_join(&config.allow)
        )));
    }
    let program = rest[0].clone();
    // A leading `~` counts as a path too: rendering expands it (`~root` becomes `/root`), so it
    // would pass this check as a name and run as a path.
    if program.contains('/') || program.starts_with('~') {
        return Err(HerdrRunError::refused(format!(
            "program must be a bare command name resolved from PATH, not a path: {}",
            quote(&program)
        )));
    }
    if !config.allows_any_program() && !config.allow.contains(&program) {
        let hint = if config.prefixes.contains(&program) {
            format!(" ({} is a wrapper prefix, not a program)", quote(&program))
        } else {
            String::new()
        };
        return Err(HerdrRunError::refused(format!(
            "program {} is not allowlisted{hint}. Allowed: {}",
            quote(&program),
            sorted_join(&config.allow)
        )));
    }

    // Expanded BEFORE any rule reads them: `git ~ foo` with HOME=daemon is `git daemon foo`, and
    // must be judged as that. A HOME that is not valid UTF-8 is a configuration error, and a home
    // holding a control character is refused like any other argument that holds one.
    let mut expanded_args = Vec::with_capacity(rest.len() - 1);
    for token in &rest[1..] {
        let expanded = expand_tilde_with(token, home)?;
        if let Some(error) = control_refusal(&expanded, "command argument") {
            return Err(error);
        }
        expanded_args.push(expanded);
    }
    let args = expanded_args.as_slice();
    for token in args {
        // `--upload-pack=/tmp/evil`, the space-separated `--upload-pack /tmp/evil`, and the
        // abbreviation `--upload-pac=/tmp/evil` all name the option.
        if config
            .deny_anywhere
            .iter()
            .any(|denied| matches_denied_option(token, denied))
        {
            return Err(HerdrRunError::refused(format!(
                "option {} is denied: it names a program for {program} to execute",
                quote(token)
            )));
        }
    }

    let deny_global = config
        .deny_global
        .get(&program)
        .map_or(&[][..], Vec::as_slice);
    let value_options = config
        .value_options
        .get(&program)
        .map_or(&[][..], Vec::as_slice);

    // Cargo accepts its "global" flags on either side of the subcommand. Check every token first,
    // including clap's attached/clustered short forms (`-Zfoo`, `-Z=foo`, `-qZfoo`).
    if program == "cargo" {
        let cargo_denied = deny_global
            .iter()
            .map(String::as_str)
            .chain(CARGO_ALWAYS_DENIED);
        for token in args {
            if let Some(denied) = cargo_denied
                .clone()
                .find(|denied| matches_denied_option(token, denied))
            {
                return Err(HerdrRunError::refused(format!(
                    "option {} is denied everywhere for cargo: it can make cargo execute arbitrary code",
                    quote(denied)
                )));
            }
        }
    }

    let mut subcommand = None;
    let mut subcommand_index = args.len();
    let mut expect_value = false;
    for (index, token) in args.iter().enumerate() {
        if expect_value {
            expect_value = false;
            continue;
        }
        if !token.starts_with('-') {
            subcommand = Some(token.clone());
            subcommand_index = index;
            break;
        }
        let (base, has_inline_value) = token
            .split_once('=')
            .map_or((token.as_str(), false), |(name, _)| (name, true));
        if let Some(denied) = deny_global
            .iter()
            .find(|denied| matches_denied_option(token, denied))
        {
            return Err(HerdrRunError::refused(format!(
                "global option {} is denied for {program}: it can make {program} execute arbitrary code",
                quote(denied)
            )));
        }
        if !has_inline_value && value_options.iter().any(|option| option == base) {
            expect_value = true;
        }
    }
    let allowed_subcommands = config.allow_subcommand.get(&program);
    if program == "cargo" && allowed_subcommands.is_none() {
        return Err(HerdrRunError::refused(
            "cargo requires an explicit allow_subcommand entry; omitting its positive list would silently admit compilation-oriented and unknown subcommands",
        ));
    }
    if let Some(allowed) = allowed_subcommands {
        let Some(candidate) = subcommand.as_deref() else {
            return Err(HerdrRunError::refused(format!(
                "{program} requires a subcommand, and only these are allowed: {}",
                sorted_join(allowed)
            )));
        };
        if !allowed.iter().any(|item| item == candidate) {
            return Err(HerdrRunError::refused(format!(
                "subcommand {} is not allowlisted. Allowed: {}. {program} compilation-oriented and unknown subcommands are deliberately excluded. Cargo must also be explicitly allowlisted because even dependency commands may execute ambiently configured helpers.",
                quote(&format!("{program} {candidate}")),
                sorted_join(allowed)
            )));
        }
    }
    if let Some(candidate) = subcommand.as_deref() {
        if config
            .deny_subcommand
            .get(&program)
            .is_some_and(|denied| denied.iter().any(|item| item == candidate))
        {
            return Err(HerdrRunError::refused(format!(
                "subcommand {} is denied: it defines or runs arbitrary code",
                quote(&format!("{program} {candidate}"))
            )));
        }
        // Options that are harmless elsewhere but name a program after this one subcommand
        // (`git clone -u CMD`). Every token after the subcommand is judged.
        let scoped = config
            .deny_subcommand_option
            .get(&program)
            .and_then(|entries| entries.get(candidate))
            .map_or(&[][..], Vec::as_slice);
        for token in &args[subcommand_index + 1..] {
            if scoped
                .iter()
                .any(|denied| matches_denied_option(token, denied))
            {
                return Err(HerdrRunError::refused(format!(
                    "option {} is denied for {}: it names a program for {program} to execute",
                    quote(token),
                    quote(&format!("{program} {candidate}"))
                )));
            }
        }
    }
    let rendered = prefix
        .iter()
        .chain(std::iter::once(&program))
        .chain(args)
        .map(|token| quote_word(token))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Admission {
        argv,
        prefix,
        program,
        subcommand,
        rendered,
    })
}

/// Whether `token` spells the denied option `denied`. Every `deny_*` option rule uses this.
///
/// Three spellings match: the option itself or with an attached value (`--upload-pack=CMD`); for
/// a long option (`--` and at least one more character), any abbreviation of it with or without a
/// value (`--upload-pac=CMD`, `--u`), because Git's option parser accepts any prefix that is
/// unambiguous among the subcommand's options and which those are depends on the subcommand and
/// the Git version; and for a one-letter short option, the letter anywhere in a short-option
/// cluster (`-qu/tmp/x`, `-Zfoo`, `-Z=foo`, `-qZfoo`), which is intentionally fail-closed. A full
/// option that merely starts the same way (`--exec-path` against `--exec`) is a different option.
fn matches_denied_option(token: &str, denied: &str) -> bool {
    if token == denied
        || token
            .strip_prefix(denied)
            .is_some_and(|rest| rest.starts_with('='))
    {
        return true;
    }
    if denied.starts_with("--") {
        let name = token.split_once('=').map_or(token, |(name, _)| name);
        return denied.len() > 2
            && name.len() > 2
            && name.starts_with("--")
            && denied.starts_with(name);
    }
    let mut denied_chars = denied.chars();
    let Some('-') = denied_chars.next() else {
        return false;
    };
    let Some(short) = denied_chars.next() else {
        return false;
    };
    if short == '-'
        || denied_chars.next().is_some()
        || token.starts_with("--")
        || !token.starts_with('-')
    {
        return false;
    }
    token[1..].contains(short)
}

fn sorted_join(values: &[String]) -> String {
    let mut sorted: Vec<&str> = values.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(", ")
}

/// Expand a token's leading `~` or `~user` without allowing a shell to interpret the result.
///
/// The rules, identical in both editions: `~` uses `HOME` when it is set (refusing a `HOME` that
/// is not valid UTF-8, because a lossy conversion would name a different directory), and the
/// account database otherwise; `~user` asks the account database. A lookup that fails, or a home
/// directory that is not valid UTF-8, leaves the token literal. Trailing slashes are dropped from
/// the home, and an empty home expands to the remainder of the token, or `/`.
pub fn expand_tilde(token: &str) -> Result<String> {
    expand_tilde_with(token, std::env::var_os("HOME").as_deref())
}

/// [`expand_tilde`] with an explicit value for `HOME` (`None` when it is unset).
pub fn expand_tilde_with(token: &str, home: Option<&OsStr>) -> Result<String> {
    if !token.starts_with('~') {
        return Ok(token.to_owned());
    }
    let slash = token.find('/').unwrap_or(token.len());
    let user = &token[1..slash];
    let suffix = &token[slash..];
    let home = if user.is_empty() {
        match home.map(OsStr::to_os_string) {
            Some(value) => Some(OsString::into_string(value).map_err(|raw| {
                HerdrRunError::config(format!(
                    "HOME is not valid UTF-8: {}",
                    quote(&raw.to_string_lossy())
                ))
            })?),
            None => account_home_text(None),
        }
    } else {
        account_home_text(Some(user))
    };
    let Some(home) = home else {
        return Ok(token.to_owned());
    };
    let trimmed = home.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(if suffix.is_empty() {
            "/".to_owned()
        } else {
            suffix.to_owned()
        });
    }
    Ok(format!("{trimmed}{suffix}"))
}

/// The account database's home directory for `user`, or for the current uid when `None`.
///
/// `None` when there is no such account or its home is not valid UTF-8; an empty home is
/// returned as an empty string.
fn account_home_text(user: Option<&str>) -> Option<String> {
    const DEFAULT_BUFFER: usize = 16 * 1024;
    const MAX_BUFFER: usize = 16 * 1024 * 1024;

    // Besides being invalid input to `getpwnam_r`, an interior NUL could otherwise make the NSS
    // lookup apply to a different username than the policy code inspected.
    let name = match user {
        Some(user) => Some(CString::new(user).ok()?),
        None => None,
    };
    // SAFETY: `sysconf` has no pointer arguments or caller-maintained invariants. A negative result
    // means that libc has no useful size recommendation, in which case the conservative default is
    // used.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested)
            .ok()?
            .clamp(DEFAULT_BUFFER, MAX_BUFFER)
    } else {
        DEFAULT_BUFFER
    };

    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        // SAFETY: `name` (when present) is a live NUL-terminated C string. `record` points to
        // writable storage, and `buffer` is live and writable for exactly the length passed.
        // `result` is an out pointer. No field of `record` is read unless libc reports success and
        // a non-null result.
        let code = unsafe {
            match &name {
                Some(name) => libc::getpwnam_r(
                    name.as_ptr(),
                    record.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                ),
                None => libc::getpwuid_r(
                    libc::getuid(),
                    record.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                ),
            }
        };
        if code == libc::ERANGE {
            if size >= MAX_BUFFER {
                return None;
            }
            size = size.saturating_mul(2).min(MAX_BUFFER);
            continue;
        }
        if code == libc::EINTR {
            continue;
        }
        if code != 0 || result.is_null() {
            return None;
        }
        // SAFETY: the successful non-null lookup initializes `record`. `pw_dir` points into either
        // `record` or `buffer` and is NUL-terminated by the API contract. The bytes are copied into
        // an owned Vec/String before either backing allocation is dropped.
        let record = unsafe { record.assume_init() };
        if record.pw_dir.is_null() {
            return Some(String::new());
        }
        let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes().to_vec();
        return String::from_utf8(bytes).ok();
    }
}

/// Expand a leading `~` in every token and quote each, refusing terminal controls.
///
/// [`admit`] does not call this: it expands only the arguments after the program, and judges them
/// expanded. This is the same rendering for an argv that needs no policy. Rendering is fallible
/// because a tilde expansion can introduce terminal control characters through ambient account
/// data even when the original argv was clean, and because a `HOME` that is not valid UTF-8 is
/// refused rather than converted lossily.
pub fn render<S: AsRef<str>>(argv: &[S]) -> Result<String> {
    let mut quoted = Vec::with_capacity(argv.len());
    for token in argv {
        let expanded = expand_tilde(token.as_ref())?;
        if let Some(error) = control_refusal(&expanded, "command argument") {
            return Err(error);
        }
        quoted.push(quote_word(&expanded));
    }
    Ok(quoted.join(" "))
}

/// Quote one word for a POSIX shell, by the rule both editions share.
///
/// A non-empty word made only of ASCII letters, digits, and `_@%+=:,./-` is returned unchanged;
/// anything else is wrapped in single quotes, with each `'` written as `'"'"'`.
pub(crate) fn quote_word(token: &str) -> String {
    if token.is_empty() {
        return "''".to_owned();
    }
    if token.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
            )
    }) {
        return token.to_owned();
    }
    format!("'{}'", token.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn current_account() -> (String, String) {
        const MAX_BUFFER: usize = 16 * 1024 * 1024;
        // SAFETY: `getuid` has no preconditions.
        let uid = unsafe { libc::getuid() };
        let mut size = 16 * 1024;
        loop {
            let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut result = std::ptr::null_mut();
            let mut buffer = vec![0_u8; size];
            // SAFETY: every pointer names live writable storage of the advertised size. Fields are
            // read only after a successful non-null result and copied before the buffer is dropped.
            let code = unsafe {
                libc::getpwuid_r(
                    uid,
                    record.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            };
            if code == libc::ERANGE && size < MAX_BUFFER {
                size = size.saturating_mul(2).min(MAX_BUFFER);
                continue;
            }
            assert_eq!(code, 0, "getpwuid_r failed");
            assert!(!result.is_null(), "current account missing from NSS");
            // SAFETY: successful `getpwuid_r` initialized the record and both pointers remain live
            // until the strings below have been copied.
            let record = unsafe { record.assume_init() };
            assert!(!record.pw_name.is_null());
            assert!(!record.pw_dir.is_null());
            let user = unsafe { CStr::from_ptr(record.pw_name) }
                .to_str()
                .unwrap()
                .to_owned();
            let home = unsafe { CStr::from_ptr(record.pw_dir) }
                .to_str()
                .unwrap()
                .to_owned();
            return (user, home);
        }
    }

    #[test]
    fn intended_commands_are_admitted_with_program_prefix_and_subcommand() {
        let config = Config::default();
        let admission = admit("with-proxy git -C /tmp/repo log --oneline", &config).unwrap();
        assert_eq!(admission.prefix, vec!["with-proxy"]);
        assert_eq!(admission.program, "git");
        assert_eq!(admission.subcommand.as_deref(), Some("log"));
        assert!(admit("cargo fetch", &config).is_err());
    }

    #[test]
    fn tokenizer_matches_python_shlex_for_quotes_escapes_comments_and_empty_words() {
        let config = Config {
            allow: vec!["printf".into()],
            ..Config::default()
        };
        let admission = admit(
            "printf '' \"two words\" a\\ b '#literal' a#b \"x\\qx\"",
            &config,
        )
        .unwrap();
        assert_eq!(
            admission.argv,
            vec!["printf", "", "two words", "a b", "#literal", "a#b", "x\\qx"]
        );
    }

    #[test]
    fn metacharacters_are_literal_arguments_and_rendered_inert() {
        let admission = admit("git status; curl evil.example && id", &Config::default()).unwrap();
        assert_eq!(
            admission.argv,
            vec!["git", "status;", "curl", "evil.example", "&&", "id"]
        );
        assert_eq!(
            admission.rendered(),
            "git 'status;' curl evil.example '&&' id"
        );
    }

    #[test]
    fn render_is_byte_for_byte_python_shlex_quote_style() {
        assert_eq!(
            render(&["git", "commit", "-m", "two words", "it's", ""]).unwrap(),
            "git commit -m 'two words' 'it'\"'\"'s' ''"
        );
    }

    #[test]
    fn refuses_empty_unbalanced_paths_prefix_repetition_and_unknown_programs() {
        let config = Config::default();
        for (command, fragment) in [
            (" ", "empty command"),
            ("git 'no", "No closing quotation"),
            ("git x\\", "No escaped character"),
            ("./git status", "bare command name"),
            ("with-proxy with-proxy git status", "repeated"),
            ("curl x", "not allowlisted"),
        ] {
            let error = admit(command, &config).expect_err(command);
            assert_eq!(error.exit_code(), crate::error::EXIT_REFUSED);
            assert!(error.to_string().contains(fragment), "{error}");
        }
    }

    /// The wildcard turns off the PROGRAM-NAME check and nothing else.
    ///
    /// Naming each surviving rule matters more than checking that `curl` now runs: the value of an
    /// allow-everything mode a project can actually reach for is that it is still not a way to
    /// smuggle control characters into a shared pane or to execute a planted binary by path.
    #[test]
    fn the_allow_wildcard_admits_any_program_and_keeps_every_other_rule() {
        let config = Config {
            allow: vec!["*".into()],
            ..Config::default()
        };
        for (command, program) in [
            ("curl https://example.invalid", "curl"),
            ("bash -lc true", "bash"),
            ("rm -rf /x", "rm"),
        ] {
            assert_eq!(admit(command, &config).expect(command).program, program);
        }
        assert_eq!(
            admit("with-proxy curl https://example.invalid", &config)
                .unwrap()
                .prefix,
            vec!["with-proxy"]
        );
        for (command, fragment) in [
            ("/bin/curl x", "bare command name"),
            ("./curl x", "bare command name"),
            ("~root x", "bare command name"),
            ("~ x", "bare command name"),
            ("curl x\u{0007}", "terminal control U+0007"),
            ("with-proxy with-proxy curl x", "repeated"),
            ("git --exec-path=/tmp/evil status", "is denied for git"),
            ("git push --receive-pack=/tmp/evil origin", "is denied"),
            (
                "cargo build --release",
                "subcommand \"cargo build\" is not allowlisted",
            ),
            ("with-proxy", "no program"),
        ] {
            let error = admit(command, &config).expect_err(command);
            assert_eq!(error.exit_code(), crate::error::EXIT_REFUSED, "{command}");
            assert!(error.to_string().contains(fragment), "{command}: {error}");
        }

        // The wildcard does not reach cargo's own guard: without a positive subcommand list,
        // cargo stays refused outright rather than inheriting "everything is allowed".
        let no_cargo_list = Config {
            allow: vec!["*".into()],
            allow_subcommand: std::collections::BTreeMap::new(),
            ..Config::default()
        };
        let error = admit("cargo fetch", &no_cargo_list).expect_err("cargo fetch");
        assert!(
            error
                .to_string()
                .contains("requires an explicit allow_subcommand entry"),
            "{error}"
        );
    }

    #[test]
    fn deny_rules_observe_global_subcommand_and_anywhere_scopes() {
        let config = Config::default();
        for command in [
            "git -c core.pager=sh log",
            "git --exec-path=/tmp/evil status",
            "gh extension exec evil",
            "git push --receive-pack=/tmp/evil origin main",
        ] {
            assert!(admit(command, &config).is_err(), "{command}");
        }
        assert_eq!(
            admit("git notes add -c deadbeef", &config)
                .unwrap()
                .subcommand
                .as_deref(),
            Some("notes")
        );
        assert_eq!(
            admit("git -C /tmp/repo log", &config)
                .unwrap()
                .subcommand
                .as_deref(),
            Some("log")
        );
    }

    #[test]
    fn distinct_prefixes_can_chain_but_each_can_appear_only_once() {
        let config = Config {
            prefixes: vec!["outer".into(), "inner".into()],
            ..Config::default()
        };
        assert_eq!(
            admit("outer inner git status", &config).unwrap().prefix,
            vec!["outer", "inner"]
        );
        assert!(admit("outer inner outer git status", &config).is_err());
    }

    #[test]
    fn leading_tilde_expands_and_remains_quoted() {
        let home = std::env::var("HOME").expect("test environment has HOME");
        let rendered = render(&["git", "-C", "~/dir; rm -rf /", "status"]).unwrap();
        assert!(rendered.contains(&home));
        assert!(rendered.contains('\''));
        assert!(!rendered.contains("~/"));
        assert_eq!(expand_tilde("a~b").unwrap(), "a~b");
        assert_eq!(expand_tilde("--opt=~/x").unwrap(), "--opt=~/x");
    }

    #[test]
    fn named_current_user_expands_through_nss() {
        let (user, home) = current_account();
        let expected_home = home.trim_end_matches('/');
        let expected = if expected_home.is_empty() {
            "/probe".to_owned()
        } else {
            format!("{expected_home}/probe")
        };
        assert_eq!(expand_tilde(&format!("~{user}/probe")).unwrap(), expected);
    }

    #[test]
    fn unknown_or_interior_nul_user_is_left_literal() {
        let unknown = format!("~__herdr_run_missing_user_{}/probe", std::process::id());
        assert_eq!(expand_tilde(&unknown).unwrap(), unknown);
        let nul = "~bad\0user/probe";
        assert_eq!(expand_tilde(nul).unwrap(), nul);
    }

    #[test]
    fn custom_policy_can_widen_narrow_and_replace_defaults() {
        let mut config = Config {
            allow: vec!["cargo".into()],
            ..Config::default()
        };
        config.prefixes.clear();
        assert_eq!(admit("cargo fetch", &config).unwrap().program, "cargo");
        assert!(admit("git status", &config).is_err());
        assert!(admit("with-proxy cargo fetch", &config).is_err());
    }

    #[test]
    fn cargo_is_disabled_by_default_and_explicit_opt_in_is_fail_closed() {
        assert!(admit("cargo fetch", &Config::default()).is_err());
        let mut config = Config::default();
        config.allow.push("cargo".to_owned());
        for command in [
            "cargo fetch",
            "with-proxy cargo fetch --manifest-path /w/Cargo.toml",
            "cargo update -p serde",
            "cargo generate-lockfile",
            "cargo vendor",
            "cargo metadata",
        ] {
            assert!(admit(command, &config).is_ok(), "{command}");
        }
        for command in [
            "cargo",
            "cargo build",
            "cargo test",
            "cargo run",
            "cargo bench",
            "cargo install ripgrep",
            "cargo rustc",
            "cargo clippy",
            "cargo doc",
            "cargo miri test",
            "cargo something-new",
            "with-proxy cargo build",
            "cargo --config build.rustc-wrapper=/tmp/evil fetch",
            "cargo --config=build.rustc-wrapper=/tmp/evil fetch",
            "cargo fetch --config build.rustc-wrapper=/tmp/evil",
            "cargo fetch --config=build.rustc-wrapper=/tmp/evil",
            "cargo -Z unstable-options fetch",
            "cargo -Zunstable-options fetch",
            "cargo fetch -Z unstable-options",
            "cargo fetch -Zunstable-options",
            "cargo fetch -Z=unstable-options",
            "cargo fetch -qZunstable-options",
        ] {
            assert!(admit(command, &config).is_err(), "{command}");
        }
    }

    #[test]
    fn cargo_opt_in_cannot_drop_its_positive_subcommand_policy() {
        let mut config = Config::default();
        config.allow.push("cargo".to_owned());
        config.allow_subcommand.clear();
        config
            .allow_subcommand
            .insert("custom-tool".to_owned(), vec!["inspect".to_owned()]);

        let error = admit("cargo build", &config).unwrap_err();
        assert!(error
            .message()
            .contains("requires an explicit allow_subcommand"));
    }

    #[test]
    fn cargo_minimum_injection_denies_cannot_be_removed() {
        let mut config = Config {
            allow: vec!["cargo".to_owned()],
            ..Config::default()
        };
        config.deny_global.clear();
        for command in ["cargo fetch --config=x", "cargo fetch -Zunstable-options"] {
            let error = admit(command, &config).unwrap_err();
            assert!(error.message().contains("denied"));
        }
    }

    #[test]
    fn project_can_explicitly_widen_the_cargo_subcommand_policy() {
        let mut config = Config::default();
        config.allow.push("cargo".to_owned());
        config.allow_subcommand.insert(
            "cargo".to_owned(),
            vec!["fetch".to_owned(), "build".to_owned()],
        );
        assert!(admit("cargo build", &config).is_ok());
    }

    #[test]
    fn every_terminal_control_range_is_refused_before_pty_injection() {
        for codepoint in (0x00..=0x1f).chain(0x7f..=0x9f) {
            let character = char::from_u32(codepoint).unwrap();
            let command = format!("git status{character}evil");
            let error = admit(&command, &Config::default()).expect_err("control must fail");
            assert_eq!(
                error.exit_code(),
                crate::error::EXIT_REFUSED,
                "U+{codepoint:04X}"
            );
            assert!(error.to_string().contains("terminal control"));

            let error = render(&["git", command.as_str()]).expect_err("render must fail");
            assert_eq!(error.exit_code(), crate::error::EXIT_REFUSED);
        }
    }

    /// `HOME` can no longer carry an option, a subcommand, or a program past the policy: every
    /// argument is judged after `~` expansion, as it will run. Each repro below was ALLOWED before.
    #[test]
    fn home_cannot_smuggle_an_option_or_subcommand_past_the_policy() {
        let config = Config::default();
        let refused = |home: &str, command: &str| {
            admit_with_home(command, &config, Some(OsStr::new(home)))
                .expect_err("the expanded argv must be judged")
        };
        let error = refused("--exec-path=/tmp/evil", "git ~ foo");
        assert_eq!(error.exit_code(), crate::error::EXIT_REFUSED);
        assert!(error.to_string().contains("--exec-path"), "{error}");
        let error = refused("daemon", "git ~ foo");
        assert!(error.to_string().contains("git daemon"), "{error}");
        let error = refused("--upload-pack=/tmp/evil", "git fetch ~");
        assert!(error.to_string().contains("--upload-pack"), "{error}");
        let error = refused("-c", "git ~ core.pager=evil log");
        assert!(error.to_string().contains("\"-c\""), "{error}");
        for home in ["git", "with-proxy"] {
            let error = refused(home, "~ status");
            assert!(error.to_string().contains("not a path"), "{error}");
        }
        // And what is admitted is exactly what is rendered.
        let admission =
            admit_with_home("git -C ~/repo status", &config, Some(OsStr::new("/home/u"))).unwrap();
        assert_eq!(admission.rendered(), "git -C /home/u/repo status");
        assert_eq!(admission.subcommand.as_deref(), Some("status"));
        assert_eq!(admission.argv, vec!["git", "-C", "~/repo", "status"]);
    }

    /// The shared configuration-and-admission corpus, which the other edition's suite loads too.
    #[test]
    fn golden_policy_cases() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/policy_cases.json")).expect("corpus");
        for case in document["cases"].as_array().expect("cases") {
            let name = case["name"].as_str().unwrap();
            let config = match case["config"].as_str() {
                None => Ok(Config::default()),
                Some(text) => match crate::yaml::core_load(text) {
                    Err(error) => Err(format!("invalid YAML: {error}")),
                    Ok(document) => crate::config::parse_config(&document, None, "/".to_owned())
                        .map_err(|error| error.message().to_owned()),
                },
            };
            let outcome = match config {
                Err(message) => serde_json::json!({ "config_error": message }),
                Ok(config) => {
                    let home = case["home"].as_str().map(OsStr::new);
                    let mut outcome =
                        golden_admission(&config, home, case["command"].as_str().unwrap());
                    if let Some(warning) = crate::config::missing_defaults_warning(&config) {
                        outcome["warning"] = serde_json::Value::String(warning);
                    }
                    outcome
                }
            };
            assert_eq!(outcome, case["expect"], "{name}");
        }
    }

    /// The admission, refusal, or configuration error one command meets, as the corpus writes it.
    pub(crate) fn golden_admission(
        config: &Config,
        home: Option<&OsStr>,
        command: &str,
    ) -> serde_json::Value {
        match admit_with_home(command, config, home) {
            Ok(admission) => serde_json::json!({ "allowed": {
                "program": admission.program,
                "subcommand": admission.subcommand,
                "rendered": admission.rendered(),
            }}),
            Err(error) => match error.kind() {
                crate::error::ErrorKind::Refused => {
                    serde_json::json!({ "refused": error.message() })
                }
                crate::error::ErrorKind::Config => {
                    serde_json::json!({ "config_error": error.message() })
                }
                other => panic!("{command}: unexpected {other:?}: {error}"),
            },
        }
    }
}
