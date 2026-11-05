//! Pluggable landing priority sources (lower numbers land first).

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use regex::RegexBuilder;

/// Fallback priority for sources that cannot resolve a value.
pub const DEFAULT_PRIORITY: i64 = 100;
/// Default label expression; capture group one supplies the numeric priority.
pub const DEFAULT_LABEL_PATTERN: &str = r"^(?:p|priority[:-])(\d+)$";

/// Mutable source of deterministic per-PR priorities.
pub trait PriorityProvider {
    /// Resolve a priority, where lower values sort first.
    fn priority(&mut self, pr_number: i64, labels: &[String]) -> i64;
    /// Return the most recent recoverable provider error, if any.
    fn last_error(&self) -> Option<&str> {
        None
    }
}

#[derive(Default)]
/// Provider that assigns every PR equal priority.
pub struct NonePriority;

impl PriorityProvider for NonePriority {
    fn priority(&mut self, _pr_number: i64, _labels: &[String]) -> i64 {
        0
    }
}

/// Provider that selects the lowest numeric priority captured from labels.
pub struct LabelPriority {
    regex: regex::Regex,
    default: i64,
    last_error: Option<String>,
}

impl LabelPriority {
    /// Compile a case-insensitive label expression with numeric capture group one.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|error| format!("invalid priority label pattern: {error}"))?;
        if regex.captures_len() < 2 {
            return Err("priority label pattern must contain a capture group".to_owned());
        }
        Ok(Self {
            regex,
            default: DEFAULT_PRIORITY,
            last_error: None,
        })
    }
}

impl PriorityProvider for LabelPriority {
    fn priority(&mut self, pr_number: i64, labels: &[String]) -> i64 {
        let mut best = None;
        for label in labels {
            let Some(captures) = self.regex.captures(label) else {
                continue;
            };
            if captures.get(0).is_none_or(|matched| matched.start() != 0) {
                continue;
            }
            let Some(captured) = captures.get(1) else {
                self.last_error = Some(format!(
                    "priority label pattern matched without a captured value for #{pr_number}"
                ));
                continue;
            };
            let Ok(value) = captured.as_str().parse::<i64>() else {
                self.last_error = Some(format!(
                    "priority label for #{pr_number} is not a signed 64-bit ASCII integer"
                ));
                continue;
            };
            best = Some(best.map_or(value, |current: i64| current.min(value)));
        }
        best.unwrap_or(self.default)
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

/// Provider that executes a configured command after substituting `{pr}`.
pub struct CommandPriority {
    command: String,
    wrapper: Vec<String>,
    default: i64,
    timeout: Duration,
    last_error: Option<String>,
}

impl CommandPriority {
    /// Construct a command provider with the default timeout.
    pub fn new(command: String, wrapper: Vec<String>) -> Self {
        Self {
            command,
            wrapper,
            default: DEFAULT_PRIORITY,
            timeout: Duration::from_secs(20),
            last_error: None,
        }
    }

    /// Construct a command provider with an explicit timeout.
    pub fn with_timeout(command: String, wrapper: Vec<String>, timeout: Duration) -> Self {
        Self {
            command,
            wrapper,
            default: DEFAULT_PRIORITY,
            timeout,
            last_error: None,
        }
    }
}

impl PriorityProvider for CommandPriority {
    fn priority(&mut self, pr_number: i64, _labels: &[String]) -> i64 {
        let rendered = self.command.replace("{pr}", &pr_number.to_string());
        let wrapper = self.wrapper.clone();
        run_priority_command(&wrapper, &rendered, pr_number, self)
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

fn run_priority_command(
    wrapper: &[String],
    rendered: &str,
    pr_number: i64,
    source: &mut CommandPriority,
) -> i64 {
    let mut command = if let Some((program, prefix)) = wrapper.split_first() {
        let mut command = Command::new(program);
        command.args(prefix).args(["bash", "-c", rendered]);
        command
    } else {
        let mut command = Command::new("bash");
        command.args(["-c", rendered]);
        command
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            source.last_error = Some(format!("priority command failed to start for #{pr_number}"));
            return source.default;
        }
    };
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < source.timeout => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                source.last_error =
                    Some(format!("priority command failed to start for #{pr_number}"));
                break None;
            }
        }
    };
    let stdout = String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default())
        .trim()
        .to_owned();
    let _stderr = stderr_reader.join().unwrap_or_default();
    let Some(status) = status else {
        if source.last_error.is_none() {
            source.last_error = Some(format!("priority command timed out for #{pr_number}"));
        }
        return source.default;
    };
    if !status.success() {
        source.last_error = Some(format!(
            "priority command for #{pr_number} exited {}",
            status.code().unwrap_or(-1)
        ));
        return source.default;
    }
    if stdout.is_empty() {
        source.last_error = Some(format!(
            "priority command for #{pr_number} produced empty output"
        ));
        return source.default;
    }
    match stdout.parse() {
        Ok(value) => value,
        Err(_) => {
            source.last_error = Some(format!(
                "priority command for #{pr_number} did not print a signed 64-bit ASCII integer"
            ));
            source.default
        }
    }
}

/// Construct a priority provider from the `none`, `labels`, or command-backed source name.
pub fn make_priority_provider(
    source: &str,
    label_pattern: &str,
    command: &str,
    wrapper: &[String],
) -> Result<Box<dyn PriorityProvider>, String> {
    match source {
        "none" => Ok(Box::new(NonePriority)),
        "labels" => Ok(Box::new(LabelPriority::new(label_pattern)?)),
        "command" if command.trim().is_empty() => {
            Err("command priority source requires a non-empty command".to_owned())
        }
        "command" => Ok(Box::new(CommandPriority::new(
            command.to_owned(),
            wrapper.to_vec(),
        ))),
        _ => Err(format!(
            "unknown priority source {source:?} (want none|labels|command)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_choose_most_urgent() {
        let mut source = LabelPriority::new(DEFAULT_LABEL_PATTERN).unwrap();
        assert_eq!(source.priority(1, &["p7".into(), "priority:2".into()]), 2);
        assert_eq!(source.priority(1, &["other".into()]), DEFAULT_PRIORITY);
        let mut custom = LabelPriority::new(r"(\d+)").unwrap();
        assert_eq!(custom.priority(1, &["x7".into()]), DEFAULT_PRIORITY);
        assert_eq!(custom.priority(1, &["7x".into()]), 7);

        assert!(LabelPriority::new(r"^p[0-9]+$").is_err());
        let mut overflow = LabelPriority::new(r"^p(.+)$").unwrap();
        assert_eq!(
            overflow.priority(8, &["p9223372036854775808".into()]),
            DEFAULT_PRIORITY
        );
        assert!(overflow
            .last_error()
            .unwrap()
            .contains("signed 64-bit ASCII"));
    }

    #[test]
    fn command_substitutes_pr_and_fails_loudly_to_default() {
        let mut source = CommandPriority::new("printf '%s' '{pr}'".into(), vec![]);
        assert_eq!(source.priority(7, &[]), 7);
        assert!(source.last_error().is_none());

        let mut bad = CommandPriority::new("printf nope".into(), vec![]);
        assert_eq!(bad.priority(9, &[]), DEFAULT_PRIORITY);
        assert!(bad.last_error().unwrap().contains("signed 64-bit ASCII"));

        let mut overflow = CommandPriority::new("printf 9223372036854775808".into(), vec![]);
        assert_eq!(overflow.priority(9, &[]), DEFAULT_PRIORITY);
        assert!(overflow
            .last_error()
            .unwrap()
            .contains("signed 64-bit ASCII"));

        let mut slow =
            CommandPriority::with_timeout("sleep 1".into(), vec![], Duration::from_millis(20));
        assert_eq!(slow.priority(10, &[]), DEFAULT_PRIORITY);
        assert!(slow.last_error().unwrap().contains("timed out"));

        assert!(make_priority_provider("command", DEFAULT_LABEL_PATTERN, "  ", &[]).is_err());
    }
}
