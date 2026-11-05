//! Fail-closed readiness checks for a Herdr pane.
//!
//! The foreground process group is authoritative: an idle shell must own the terminal's
//! foreground group alone.  Rendered prompt text is an independent veto for a human's
//! unsubmitted command line.  Failure to infer a prompt is reported as abstention rather than
//! pretending that the text signal succeeded.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::client::{current_account_home, HerdrApi, ProcessInfo};
use crate::config::Config;
use crate::error::Result;
use crate::textfmt::{is_blank, quote, rstrip_space, strip_space};

/// Foreground-process evidence about whether the pane's shell is idle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSignal {
    /// True only when the known shell alone owns the foreground process group.
    pub idle: bool,
    /// Human-readable explanation of the verdict.
    pub reason: String,
    /// PID Herdr reports for the interactive shell.
    pub shell_pid: i64,
    /// Foreground process-group ID Herdr reports for the pane.
    pub foreground_pgid: i64,
    /// Names of the processes in the foreground process group.
    pub foreground: Vec<String>,
}

/// Prompt-text evidence, used only to veto an otherwise idle pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSignal {
    /// One of `clean`, `dirty`, or `abstain`.
    pub verdict: String,
    /// Human-readable explanation of the verdict.
    pub reason: String,
    /// Prompt suffix searched for, if one was available.
    pub tail: Option<String>,
    /// Last nonblank rendered line, if one was available.
    pub last_line: Option<String>,
}

/// Combined process and prompt-text readiness evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Readiness {
    /// True only when the process signal is idle and prompt text does not veto it.
    pub ready: bool,
    /// Short combined-verdict explanation.
    pub reason: String,
    /// Foreground-process observation.
    pub process: ProcessSignal,
    /// Prompt-text observation.
    pub prompt: PromptSignal,
}

impl Readiness {
    /// Render one audit-friendly line naming both signals and their explanations.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "process={} ({}); prompt={} ({})",
            if self.process.idle { "idle" } else { "busy" },
            self.process.reason,
            self.prompt.verdict,
            self.prompt.reason
        )
    }
}

/// Return the last uncommented `PS1` or `PROMPT` assignment in shell RC text.
#[must_use]
pub fn parse_ps1(rc_text: &str) -> Option<String> {
    let mut raw = None;
    for captures in ps1_assignment().captures_iter(rc_text) {
        raw = captures.get(1).map(|value| value.as_str().to_owned());
    }
    let raw = raw?;
    let bytes = raw.as_bytes();
    if bytes.len() >= 2
        && bytes.first() == bytes.last()
        && matches!(bytes.first(), Some(b'\'' | b'"'))
    {
        Some(raw[1..raw.len() - 1].to_owned())
    } else {
        Some(raw)
    }
}

/// Extract the fixed literal suffix that a rendered prompt ends with.
#[must_use]
pub fn prompt_tail_of(ps1: &str) -> Option<String> {
    let visible = zero_width().replace_all(ps1, "");
    let mut last_dynamic_end = 0;
    for matched in dynamic().find_iter(&visible) {
        // `\$` is the common literal prompt terminator, so retain it in the fixed suffix.
        last_dynamic_end = if matched.as_str() == r"\$" {
            matched.start()
        } else {
            matched.end()
        };
    }
    let tail = visible[last_dynamic_end..].replace(r"\$", "$");
    (!strip_space(&tail).is_empty()).then_some(tail)
}

/// Resolve the configured prompt tail, or infer it from account shell startup files.
///
/// Supplying `home` is intended for deterministic tests.  Production callers pass `None`, which
/// reads the current user's account-database home and never consults caller-controlled `HOME`.
#[must_use]
pub fn infer_prompt_tail(config: &Config, home: Option<&Path>) -> Option<String> {
    if let Some(tail) = &config.prompt_tail {
        return Some(tail.clone());
    }
    let account_home;
    let base = match home {
        Some(home) => home,
        None => {
            account_home = current_account_home().ok()?;
            &account_home
        }
    };
    for name in [".bashrc", ".zshrc", ".bash_profile", ".profile"] {
        let Ok(bytes) = fs::read(base.join(name)) else {
            continue;
        };
        // CRLF and CR line ends read as LF, so a file saved by a Windows editor still names its
        // prompt and the dirty-line veto keeps working.
        let text = String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let Some(ps1) = parse_ps1(&text) else {
            continue;
        };
        if let Some(tail) = prompt_tail_of(&ps1) {
            return Some(tail);
        }
    }
    None
}

/// Judge pane idleness from the foreground process group.
#[must_use]
pub fn assess_process(info: &ProcessInfo, config: &Config) -> ProcessSignal {
    let names = info
        .foreground
        .iter()
        .map(|(_, name, _)| name.clone())
        .collect::<Vec<_>>();
    let signal = |idle: bool, reason: String| ProcessSignal {
        idle,
        reason,
        shell_pid: info.shell_pid,
        foreground_pgid: info.foreground_pgid,
        foreground: names.clone(),
    };
    if info.foreground_pgid != info.shell_pid {
        let running = if names.is_empty() {
            "unknown".to_owned()
        } else {
            names.join(", ")
        };
        return signal(
            false,
            format!(
                "foreground pgid {} != shell pid {}; running: {running}",
                info.foreground_pgid, info.shell_pid
            ),
        );
    }
    if info.foreground.len() != 1 {
        return signal(
            false,
            format!(
                "{} foreground processes, expected only the shell",
                info.foreground.len()
            ),
        );
    }
    let (pid, name, _) = &info.foreground[0];
    if *pid != info.shell_pid {
        return signal(
            false,
            format!(
                "foreground process {pid} ({name}) is not the shell {}",
                info.shell_pid
            ),
        );
    }
    if !config.shells.iter().any(|shell| shell == name) {
        return signal(
            false,
            format!("foreground process is {}, not a known shell", quote(name)),
        );
    }
    signal(
        true,
        format!(
            "shell {name} ({}) owns the foreground group",
            info.shell_pid
        ),
    )
}

/// Judge the last rendered line as `clean`, `dirty`, or `abstain`.
#[must_use]
pub fn assess_prompt(text: &str, tail: Option<&str>) -> PromptSignal {
    let Some(tail) = tail else {
        return prompt_signal(
            "abstain",
            "no prompt tail configured or inferable",
            None,
            None,
        );
    };
    // Lines are split on line feeds only, with a trailing carriage return dropped: a Unicode line
    // separator inside a half-typed command must not hide the command from the veto.
    let last = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .rfind(|line| !is_blank(line))
        .map(str::to_owned);
    let Some(last) = last else {
        return prompt_signal(
            "abstain",
            "pane has no rendered output yet",
            Some(tail),
            None,
        );
    };
    // Herdr strips trailing spaces from rendered lines, so `$ ` must be compared as `$`.
    let needle = rstrip_space(tail);
    if needle.is_empty() {
        return prompt_signal(
            "abstain",
            "prompt tail is whitespace only",
            Some(tail),
            Some(&last),
        );
    }
    let stripped_last = rstrip_space(&last);
    let occurrences = stripped_last.match_indices(needle).count();
    if occurrences == 1 && stripped_last.ends_with(needle) {
        return prompt_signal(
            "clean",
            &format!("last line ends with prompt tail {}", quote(needle)),
            Some(tail),
            Some(&last),
        );
    }
    if occurrences > 0 {
        let characters: Vec<char> = stripped_last.chars().collect();
        let excerpt: String = characters[characters.len().saturating_sub(80)..]
            .iter()
            .collect();
        let reason = if occurrences > 1 {
            format!(
                "prompt tail {} occurs {occurrences} times; possible text follows an earlier prompt: {}",
                quote(needle),
                quote(&excerpt)
            )
        } else {
            format!(
                "text typed after prompt tail {}: {}",
                quote(needle),
                quote(&excerpt)
            )
        };
        return prompt_signal("dirty", &reason, Some(tail), Some(&last));
    }
    prompt_signal(
        "abstain",
        &format!("prompt tail {} not found in last line", quote(needle)),
        Some(tail),
        Some(&last),
    )
}

/// Take one readiness reading without sleeping or retrying.
pub fn assess<A: HerdrApi + ?Sized>(
    client: &A,
    pane_id: &str,
    config: &Config,
    prompt_tail: Option<&str>,
    read_lines: usize,
) -> Result<Readiness> {
    let process = assess_process(&client.process_info(pane_id)?, config);
    let prompt = if config.readiness == "process" {
        prompt_signal(
            "abstain",
            "prompt check disabled (readiness: process)",
            prompt_tail,
            None,
        )
    } else {
        assess_prompt(
            &client.read(pane_id, "recent-unwrapped", Some(read_lines))?,
            prompt_tail,
        )
    };
    let (ready, reason) = if !process.idle {
        (false, "pane is busy")
    } else if prompt.verdict == "dirty" {
        (false, "pane has an unsubmitted command line")
    } else {
        (true, "pane is idle at a shell prompt")
    };
    Ok(Readiness {
        ready,
        reason: reason.to_owned(),
        process,
        prompt,
    })
}

fn prompt_signal(
    verdict: &str,
    reason: &str,
    tail: Option<&str>,
    last_line: Option<&str>,
) -> PromptSignal {
    PromptSignal {
        verdict: verdict.to_owned(),
        reason: reason.to_owned(),
        tail: tail.map(str::to_owned),
        last_line: last_line.map(str::to_owned),
    }
}

fn zero_width() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\\\[.*?\\\]").expect("valid zero-width regex"))
}

fn dynamic() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"\\.|\$\([^)]*\)|\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*")
            .expect("valid prompt-dynamic regex")
    })
}

fn ps1_assignment() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?m)^[ \t]*(?:export[ \t]+)?(?:PS1|PROMPT)=(\"(?:[^\"\\]|\\.)*\"|'[^']*'|[^\t\n\x0B\x0C\r \x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}#]+)[ \t]*(?:#.*)?$"#,
        )
        .expect("valid PS1-assignment regex")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::client::Pane;
    use crate::error::HerdrRunError;

    use super::*;

    /// Prompt verdicts and rc-file prompt tails shared with the other edition.
    #[test]
    fn golden_readiness() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/readiness_cases.json"))
                .expect("golden readiness cases");
        for row in document["prompt"].as_array().expect("prompt rows") {
            let signal = assess_prompt(row[0].as_str().unwrap(), row[1].as_str());
            assert_eq!(signal.verdict, row[2], "{row:?}");
            assert_eq!(signal.reason, row[3], "{row:?}");
            assert_eq!(signal.last_line.as_deref(), row[4].as_str(), "{row:?}");
        }
        for row in document["rc_files"].as_array().expect("rc rows") {
            let payload: Vec<u8> = serde_json::from_value(row[0].clone()).unwrap();
            let root = std::env::temp_dir().join(format!(
                "herdr-run-readiness-golden-{}-{}",
                std::process::id(),
                TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join(".bashrc"), payload).unwrap();
            assert_eq!(
                infer_prompt_tail(&Config::default(), Some(&root)).as_deref(),
                row[1].as_str(),
                "{row:?}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn config() -> Config {
        Config::default()
    }

    fn idle_info(shell: &str) -> ProcessInfo {
        ProcessInfo {
            pane_id: "p1".to_owned(),
            shell_pid: 42,
            foreground_pgid: 42,
            foreground: vec![(42, shell.to_owned(), format!("-{shell}"))],
        }
    }

    #[test]
    fn ps1_parser_ignores_comments_and_last_assignment_wins() {
        let text = r#"
# PS1='ignored> '
PS1='first> '
export PS1="\u@\h:\w\$ " # selected
"#;
        assert_eq!(parse_ps1(text).as_deref(), Some(r"\u@\h:\w\$ "));
        assert_eq!(
            prompt_tail_of(r"\[\033[34m\][\u@\h \w]\n\$ ").as_deref(),
            Some("$ ")
        );
    }

    #[test]
    fn prompt_inference_uses_explicit_value_then_account_files() {
        let mut explicit = config();
        explicit.prompt_tail = Some("PROMPT> ".to_owned());
        assert_eq!(
            infer_prompt_tail(&explicit, Some(Path::new("/does/not/exist"))).as_deref(),
            Some("PROMPT> ")
        );

        let root = std::env::temp_dir().join(format!(
            "herdr-run-readiness-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".bashrc"), "PS1='\\u@\\h\\$ '\n").unwrap();
        assert_eq!(
            infer_prompt_tail(&config(), Some(&root)).as_deref(),
            Some("$ ")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_signal_requires_shell_pid_group_and_known_name() {
        assert!(assess_process(&idle_info("bash"), &config()).idle);
        let mut busy_group = idle_info("bash");
        busy_group.foreground_pgid = 99;
        assert!(!assess_process(&busy_group, &config()).idle);
        let mut extra = idle_info("bash");
        extra
            .foreground
            .push((43, "git".to_owned(), "git status".to_owned()));
        assert!(!assess_process(&extra, &config()).idle);
        assert!(!assess_process(&idle_info("python"), &config()).idle);
    }

    #[test]
    fn prompt_tail_is_clean_only_once_at_the_end() {
        assert_eq!(assess_prompt("host\n$\n", Some("$ ")).verdict, "clean");
        assert_eq!(
            assess_prompt("host\n$ git status\n", Some("$ ")).verdict,
            "dirty"
        );
        assert_eq!(
            assess_prompt("host\n$ echo $\n", Some("$ ")).verdict,
            "dirty"
        );
        assert_eq!(
            assess_prompt("host\nunknown\n", Some("$ ")).verdict,
            "abstain"
        );
        assert_eq!(assess_prompt("", None).verdict, "abstain");
    }

    struct ReadinessFake {
        info: ProcessInfo,
        text: String,
    }

    impl HerdrApi for ReadinessFake {
        fn ensure_server(&self) -> Result<bool> {
            unreachable!()
        }
        fn server_running(&self) -> bool {
            unreachable!()
        }
        fn workspace_id_for_label(&self, _label: &str) -> Result<Option<String>> {
            unreachable!()
        }
        fn create_workspace(&self, _label: &str, _cwd: &str) -> Result<(String, String, String)> {
            unreachable!()
        }
        fn tab_id_for_label(&self, _workspace_id: &str, _label: &str) -> Result<Option<String>> {
            unreachable!()
        }
        fn create_tab(&self, _workspace_id: &str, _label: &str, _cwd: &str) -> Result<String> {
            unreachable!()
        }
        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            unreachable!()
        }
        fn panes(&self, _workspace_id: Option<&str>) -> Result<Vec<Pane>> {
            unreachable!()
        }
        fn pane_exists(&self, _pane_id: &str) -> bool {
            unreachable!()
        }
        fn process_info(&self, _pane_id: &str) -> Result<ProcessInfo> {
            Ok(self.info.clone())
        }
        fn read(&self, _pane_id: &str, source: &str, lines: Option<usize>) -> Result<String> {
            assert_eq!(source, "recent-unwrapped");
            assert_eq!(lines, Some(4));
            Ok(self.text.clone())
        }
        fn run(&self, _pane_id: &str, _command: &str) -> Result<()> {
            Err(HerdrRunError::unavailable("unused"))
        }
    }

    #[test]
    fn combined_readiness_treats_prompt_as_veto_not_authority() {
        let idle = ReadinessFake {
            info: idle_info("bash"),
            text: "host\nno recognizable prompt\n".to_owned(),
        };
        let reading = assess(&idle, "p1", &config(), Some("$ "), 4).unwrap();
        assert!(reading.ready);
        assert_eq!(reading.prompt.verdict, "abstain");

        let dirty = ReadinessFake {
            info: idle_info("bash"),
            text: "host\n$ half typed\n".to_owned(),
        };
        assert!(
            !assess(&dirty, "p1", &config(), Some("$ "), 4)
                .unwrap()
                .ready
        );

        let mut busy_info = idle_info("bash");
        busy_info.foreground_pgid = 100;
        let busy = ReadinessFake {
            info: busy_info,
            text: "$\n".to_owned(),
        };
        assert!(!assess(&busy, "p1", &config(), Some("$ "), 4).unwrap().ready);
    }

    #[test]
    fn process_only_mode_does_not_read_prompt() {
        let fake = ReadinessFake {
            info: idle_info("bash"),
            text: "$ typed".to_owned(),
        };
        let mut config = config();
        config.readiness = "process".to_owned();
        let reading = assess(&fake, "p1", &config, Some("$ "), 4).unwrap();
        assert!(reading.ready);
        assert_eq!(reading.prompt.verdict, "abstain");
    }
}
