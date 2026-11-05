//! Bind a process id to the boot and start tick that make it *that* process.
//!
//! A bare PID is not an identity. Linux recycles process numbers, so "is something alive with this
//! number" is the wrong question in both directions: a recycled number makes an unrelated stranger
//! look like proof of liveness, and a same-numbered stranger makes a dead process look alive.
//! [`crate::reap`] therefore refuses to act on a PID alone and requires `(pid, boot_id,
//! start_ticks)`.
//!
//! `start_ticks` is field 22 of `/proc/<pid>/stat` — the process's start time in clock ticks since
//! boot. The kernel assigns it at fork and never changes it, so two processes that ever held the
//! same number differ in it. `boot_id` scopes that tick count: after a reboot the counter restarts,
//! so ticks alone would let a pre-reboot record match a post-reboot process.
//!
//! **Parsing `/proc/<pid>/stat` needs care.** Field 2 is the executable name in parentheses and may
//! itself contain spaces AND parentheses — `(my prog (v2))` is a legal comm. Splitting the line on
//! whitespace therefore mis-numbers every later field. The only correct split is at the LAST `)`,
//! after which the remaining whitespace-separated tokens are fields 3 onwards.
//!
//! Every function here answers "unknown" rather than guessing. An unreadable `/proc` must reach the
//! policy as UNKNOWN, never as "the process is gone", because the second reading authorises closing
//! a tab and the first does not.

use std::fs;
use std::io::ErrorKind as IoErrorKind;
use std::path::Path;

/// Path, relative to the procfs root, of the kernel's per-boot random identifier.
pub const BOOT_ID_PATH: &str = "sys/kernel/random/boot_id";

/// Index of `starttime` among the whitespace-separated fields that FOLLOW the comm field.
///
/// `stat` field 22 is the 20th token after the closing parenthesis, because tokens there begin at
/// field 3.
const START_TICKS_OFFSET: usize = 22 - 3;

/// Longest `/proc/<pid>/stat` content accepted before the read is treated as implausible.
const MAX_STAT_BYTES: usize = 8192;

/// What one live-process lookup established, as a tri-state rather than a bool.
///
/// The three cases must stay distinguishable all the way to the reaping policy:
///
/// * `gone` — the process does not exist. Only this may contribute to a STALE verdict.
/// * a `start_ticks` value — the process exists and is bound.
/// * `error` — we could not tell. UNKNOWN, never a licence to reap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellProbe {
    /// True only when `/proc/<pid>` is positively absent.
    pub gone: bool,
    /// Start tick of the live process, or `None` when it could not be read.
    pub start_ticks: Option<u64>,
    /// Why the lookup was inconclusive, or `None` when it was conclusive.
    pub error: Option<String>,
}

/// Extract field 22 (`starttime`) from one `/proc/<pid>/stat` line.
///
/// Returns `None` for anything that does not parse exactly, including a comm field with no closing
/// parenthesis and a truncated line. Guessing here would fabricate an identity.
#[must_use]
pub fn parse_start_ticks(stat_text: &str) -> Option<u64> {
    let closing = stat_text.rfind(')')?;
    let tail = &stat_text[closing + 1..];
    tail.split_whitespace()
        .nth(START_TICKS_OFFSET)
        .and_then(|token| token.parse::<u64>().ok())
}

/// Read this boot's identifier, or `None` when it cannot be established.
#[must_use]
pub fn current_boot_id(proc_root: &Path) -> Option<String> {
    let text = fs::read_to_string(proc_root.join(BOOT_ID_PATH)).ok()?;
    let trimmed = crate::textfmt::strip_space(&text);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Start tick of `pid`, or `None` when it is absent or unreadable.
#[must_use]
pub fn process_start_ticks(pid: i64, proc_root: &Path) -> Option<u64> {
    probe_process(pid, proc_root).start_ticks
}

/// Look one live process up, keeping "absent" and "could not tell" apart.
#[must_use]
pub fn probe_process(pid: i64, proc_root: &Path) -> ShellProbe {
    if pid < 1 {
        return ShellProbe {
            gone: false,
            start_ticks: None,
            error: Some(format!("not a process id: {pid}")),
        };
    }
    let path = proc_root.join(pid.to_string()).join("stat");
    let payload = match fs::read(&path) {
        Ok(payload) => payload,
        // The ONE reading that may authorise reaping: the kernel says there is no such process.
        Err(error) if error.kind() == IoErrorKind::NotFound => {
            return ShellProbe {
                gone: true,
                start_ticks: None,
                error: None,
            }
        }
        Err(error) => {
            return ShellProbe {
                gone: false,
                start_ticks: None,
                error: Some(format!("cannot read {}: {error}", path.display())),
            }
        }
    };
    if payload.len() > MAX_STAT_BYTES {
        return ShellProbe {
            gone: false,
            start_ticks: None,
            error: Some(format!("{} is implausibly long", path.display())),
        };
    }
    match parse_start_ticks(&String::from_utf8_lossy(&payload)) {
        Some(start_ticks) => ShellProbe {
            gone: false,
            start_ticks: Some(start_ticks),
            error: None,
        },
        None => ShellProbe {
            gone: false,
            start_ticks: None,
            error: Some(format!("cannot parse start ticks from {}", path.display())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_ticks_come_from_field_22() {
        // Seven tokens follow the comm (fields 3..9), then the numbers 10..22 in order, so the
        // value standing in field 22 is literally 22. Anything else means the numbering slipped.
        let numbers: Vec<String> = (10..=22).map(|value| value.to_string()).collect();
        let line = format!(
            "1234 (bash) S 1 1234 1234 0 -1 4194304 {}",
            numbers.join(" ")
        );
        assert_eq!(parse_start_ticks(&line), Some(22));
    }

    #[test]
    fn a_comm_with_spaces_and_parentheses_does_not_shift_the_fields() {
        let tail: Vec<String> = (3..40).map(|value| value.to_string()).collect();
        let honest = format!("77 (bash) S {}", tail.join(" "));
        let hostile = format!("77 (my prog (v2)) S {}", tail.join(" "));
        assert_eq!(parse_start_ticks(&hostile), parse_start_ticks(&honest));
    }

    #[test]
    fn a_truncated_stat_line_is_unknown_not_a_guess() {
        assert_eq!(parse_start_ticks("77 (bash) S 1 2 3"), None);
        assert_eq!(parse_start_ticks("no parenthesis here"), None);
        let junk = vec!["x"; 30].join(" ");
        assert_eq!(parse_start_ticks(&format!("77 (bash) S {junk}")), None);
    }

    #[test]
    fn an_absent_process_is_gone_and_an_unreadable_one_is_not() {
        let root = std::env::temp_dir().join(format!("herdr-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create probe root");
        let absent = probe_process(4242, &root);
        assert!(absent.gone && absent.error.is_none());

        fs::create_dir_all(root.join("77")).expect("create pid directory");
        fs::write(root.join("77").join("stat"), "garbage\n").expect("write stat");
        let unreadable = probe_process(77, &root);
        assert!(!unreadable.gone);
        assert!(unreadable.error.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn this_process_can_be_bound_against_the_real_proc() {
        let pid = i64::from(std::process::id());
        let probe = probe_process(pid, Path::new("/proc"));
        assert!(!probe.gone && probe.error.is_none());
        assert!(probe.start_ticks.is_some_and(|ticks| ticks > 0));
    }

    #[test]
    fn a_negative_pid_is_an_error_rather_than_a_death_certificate() {
        let probe = probe_process(-1, Path::new("/proc"));
        assert!(!probe.gone);
        assert!(probe.error.is_some());
    }

    #[test]
    fn a_pid_of_zero_is_an_error_rather_than_a_death_certificate() {
        let probe = probe_process(0, Path::new("/proc"));
        assert!(!probe.gone);
        assert!(probe
            .error
            .is_some_and(|error| error.contains("not a process id")));
    }

    #[test]
    fn a_stat_entry_that_cannot_be_opened_is_unknown_not_gone() {
        // The READ-error arm, which is a different branch from the parse-error arm above. A
        // directory where the kernel puts a file fails the read with something that is emphatically
        // NOT NotFound; reporting that as `gone` would hand the policy a death certificate written
        // by an EACCES.
        let root =
            std::env::temp_dir().join(format!("herdr-identity-unopenable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("88").join("stat")).expect("create stat directory");
        let probe = probe_process(88, &root);
        assert!(
            !probe.gone,
            "an unopenable /proc entry must not authorise reaping"
        );
        assert!(probe
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cannot read")));
        assert!(probe.start_ticks.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_implausibly_long_stat_entry_is_unknown_not_gone() {
        let root = std::env::temp_dir().join(format!("herdr-identity-huge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("99")).expect("create pid directory");
        let padding = "0 ".repeat(40_000);
        fs::write(
            root.join("99").join("stat"),
            format!("99 (bash) S {padding}\n"),
        )
        .expect("write stat");
        let probe = probe_process(99, &root);
        assert!(
            !probe.gone,
            "an oversized /proc entry must not authorise reaping"
        );
        assert!(probe
            .error
            .as_deref()
            .is_some_and(|error| error.contains("implausibly long")));
        assert!(probe.start_ticks.is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
