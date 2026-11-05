//! Cadence decisions and the crash-safe fired-state store.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::model::{Reminder, EVERY_TICK};
use crate::text::{is_whitespace, split_lines, trim};

/// Namespace reserved for tick-hub's own persisted diagnostic state.
pub const INTERNAL_STATE_PREFIX: &str = "__tick_hub_internal__.";
/// Prefix for consecutive unresolved-render accounting.
pub const UNRESOLVED_RENDER_STATE_PREFIX: &str = "__tick_hub_internal__.unresolved_render.";
/// Suffix for the consecutive unresolved-render count.
pub const UNRESOLVED_RENDER_COUNT_SUFFIX: &str = ".count";
/// Suffix for the first unresolved-render epoch.
pub const UNRESOLVED_RENDER_FIRST_SUFFIX: &str = ".first_failure_epoch";

/// Return the reserved `(count, first-failure-epoch)` keys for one reminder.
pub fn unresolved_render_state_keys(name: &str) -> (String, String) {
    let base = format!("{UNRESOLVED_RENDER_STATE_PREFIX}{name}");
    (
        format!("{base}{UNRESOLVED_RENDER_COUNT_SUFFIX}"),
        format!("{base}{UNRESOLVED_RENDER_FIRST_SUFFIX}"),
    )
}

/// The most recent absolute instant at or before `now` for a phased cadence.
///
/// The instants are `offset, offset + cadence, offset + 2*cadence, ...`. Derived from the clock
/// alone, so a restart mid-cycle and a replay at a pinned `now` land on the same instant.
pub fn scheduled_instant(cadence_secs: i64, offset_secs: i64, now: i64) -> i64 {
    now.saturating_sub(offset_secs).div_euclid(cadence_secs) * cadence_secs + offset_secs
}

/// Return whether a reminder should be checked at `now`.
///
/// `offset_secs` phases the cadence against absolute time; `window_secs` bounds how long after
/// that instant the reminder may still be offered. With both `None` this is the plain elapsed
/// rule, unchanged.
///
/// A reminder that was cut off is still due: dueness is decided from the last-fired epoch, and a
/// reminder that did not complete never records one.
pub fn is_due(
    name: &str,
    cadence_secs: i64,
    now: i64,
    last_fired: &BTreeMap<String, i64>,
    offset_secs: Option<i64>,
    window_secs: Option<i64>,
) -> bool {
    if cadence_secs <= EVERY_TICK {
        return true;
    }
    let Some(last) = last_fired.get(name).copied() else {
        // Never fired is due, phase or no phase. The pending report calls this with an empty
        // fired-state precisely so every reminder answers "due"; narrowing it here would
        // quietly shrink that report.
        return true;
    };
    let Some(offset) = offset_secs else {
        return now.saturating_sub(last) >= cadence_secs;
    };
    let instant = scheduled_instant(cadence_secs, offset, now);
    if last >= instant {
        return false;
    }
    match window_secs {
        Some(window) => now.saturating_sub(instant) < window,
        None => true,
    }
}

/// Return due reminders in registration order.
pub fn due_reminders<'a>(
    reminders: &'a [Reminder],
    now: i64,
    last_fired: &BTreeMap<String, i64>,
) -> Vec<&'a Reminder> {
    reminders
        .iter()
        .filter(|reminder| {
            is_due(
                &reminder.name,
                reminder.cadence_secs,
                now,
                last_fired,
                reminder.cadence_offset_secs,
                reminder.cadence_window_secs,
            )
        })
        .collect()
}

/// Load valid `key=epoch-or-count` lines. Missing, unreadable, and malformed data is ignored.
pub fn load_fired_state(path: &Path) -> BTreeMap<String, i64> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let mut state = BTreeMap::new();
    for raw_line in split_lines(&text) {
        let line = trim(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty()
            || key.chars().any(is_whitespace)
            || value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        if let Ok(epoch) = value.parse::<i64>() {
            state.insert(key.to_string(), epoch);
        }
    }
    state
}

/// Atomically write a sorted fired-state file using a same-directory temporary file.
pub fn persist_fired_state(path: &Path, state: &BTreeMap<String, i64>) -> io::Result<()> {
    for (key, value) in state {
        if key.is_empty() || key.contains('=') || key.chars().any(is_whitespace) || *value < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid fired-state entry {key:?}={value}"),
            ));
        }
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut temporary_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("state"))
        .to_os_string();
    temporary_name.push(".tmp");
    let temporary = path.with_file_name(temporary_name);
    let mut text =
        String::from("# tick-hub fired-state — reminder=last_fired_epoch (managed by tick-hub)\n");
    text.push_str(&format!(
        "# {INTERNAL_STATE_PREFIX}* entries are reserved retry diagnostics\n"
    ));
    for (key, value) in state {
        text.push_str(&format!("{key}={value}\n"));
    }
    fs::write(&temporary, text)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Emit, Reminder};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tick-hub-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn exact_cadence_boundary_is_due() {
        let fired = BTreeMap::from([("r".to_string(), 1_000)]);
        assert!(is_due("r", 600, 1_600, &fired, None, None));
        assert!(!is_due("r", 600, 1_599, &fired, None, None));
        assert!(is_due("r", 0, 1_000, &fired, None, None));
    }

    #[test]
    fn due_reminders_preserve_registration_order() {
        let mut a = Reminder::new("a", Emit::note("a"));
        a.cadence_secs = 0;
        let mut b = Reminder::new("b", Emit::note("b"));
        b.cadence_secs = 3_600;
        let c = Reminder::new("c", Emit::note("c"));
        let fired = BTreeMap::from([("b".to_string(), 1_000)]);
        let reminders = [a, b, c];
        let names: Vec<_> = due_reminders(&reminders, 1_500, &fired)
            .into_iter()
            .map(|reminder| reminder.name.as_str())
            .collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn fired_state_round_trips_and_ignores_garbage() {
        let root = temporary_path("cadence");
        let path = root.join("sub/state");
        let (count_key, first_key) = unresolved_render_state_keys("a");
        let state = BTreeMap::from([
            ("a".to_string(), 100),
            ("b".to_string(), 200),
            (count_key, 3),
            (first_key, 50),
        ]);
        persist_fired_state(&path, &state).unwrap();
        assert_eq!(load_fired_state(&path), state);
        assert!(fs::read_to_string(&path).unwrap().contains(&format!(
            "# {INTERNAL_STATE_PREFIX}* entries are reserved retry diagnostics"
        )));
        fs::write(&path, "# comment\nvalid=42\nbad line\nk=notnum\n").unwrap();
        assert_eq!(
            load_fired_state(&path),
            BTreeMap::from([("valid".into(), 42)])
        );
        assert!(persist_fired_state(&path, &BTreeMap::from([("bad key".into(), 1)]),).is_err());
        assert!(persist_fired_state(&path, &BTreeMap::from([("bad".into(), -1)]),).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fired_state_removes_temporary_file_after_rename_failure() {
        let root = temporary_path("cadence-rename-failure");
        let path = root.join("state");
        fs::create_dir_all(&path).unwrap();
        assert!(persist_fired_state(&path, &BTreeMap::from([("valid".into(), 1)])).is_err());
        assert!(!root.join("state.tmp").exists());
        let _ = fs::remove_dir_all(root);
    }

    // ---- phased cadences -------------------------------------------------------------
    // These mirror py/tests/test_tickhub_cadence_offset.py case for case. The two editions
    // must accept the same schema AND make the same transitions; a Rust parser that takes
    // the fields while the Rust due-logic ignores them would be the worse failure.

    const HOUR: i64 = 3_600;
    const HALF: i64 = 1_800;

    fn state(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn no_offset_keeps_the_elapsed_rule() {
        let st = state(&[("r", 1_000_000)]);
        for elapsed in [0, 1, HOUR - 1, HOUR, HOUR + 1, 10 * HOUR] {
            let now = 1_000_000 + elapsed;
            assert_eq!(is_due("r", HOUR, now, &st, None, None), elapsed >= HOUR);
        }
    }

    #[test]
    fn never_fired_is_due_in_both_modes() {
        let empty = BTreeMap::new();
        assert!(is_due("r", HOUR, 1_000_000, &empty, None, None));
        assert!(is_due("r", HOUR, 1_000_000, &empty, Some(0), Some(HALF)));
    }

    #[test]
    fn scheduled_instants_land_on_the_phase() {
        assert_eq!(scheduled_instant(HOUR, 0, 7 * HOUR + 5), 7 * HOUR);
        assert_eq!(scheduled_instant(HOUR, HALF, 7 * HOUR + 5), 6 * HOUR + HALF);
        assert_eq!(
            scheduled_instant(HOUR, HALF, 7 * HOUR + HALF),
            7 * HOUR + HALF
        );
    }

    #[test]
    fn each_phase_fires_once_per_hour_and_never_together() {
        let a = Reminder {
            cadence_secs: HOUR,
            cadence_offset_secs: Some(0),
            cadence_window_secs: Some(HALF),
            ..Reminder::new("phase_a", Emit::note("a"))
        };
        let b = Reminder {
            cadence_secs: HOUR,
            cadence_offset_secs: Some(HALF),
            cadence_window_secs: Some(HALF),
            ..Reminder::new("phase_b", Emit::note("b"))
        };
        let mut fired = state(&[("phase_a", 0), ("phase_b", HALF)]);
        let (mut ca, mut cb, mut together) = (0, 0, 0);
        for tick in 2..50 {
            let now = tick * HALF;
            let due: Vec<String> = due_reminders(&[a.clone(), b.clone()], now, &fired)
                .iter()
                .map(|r| r.name.clone())
                .collect();
            if due.len() == 2 {
                together += 1;
            }
            for name in due {
                if name == "phase_a" {
                    ca += 1;
                } else {
                    cb += 1;
                }
                fired.insert(name, now);
            }
        }
        assert_eq!((ca, cb, together), (24, 24, 0));
    }

    #[test]
    fn the_window_keeps_a_cut_off_reminder_on_its_own_phase() {
        let seeded = state(&[("r", 0)]);
        assert!(is_due("r", HOUR, HOUR, &seeded, Some(0), Some(HALF)));
        assert!(!is_due(
            "r",
            HOUR,
            HOUR + HALF,
            &seeded,
            Some(0),
            Some(HALF)
        ));
        assert!(is_due("r", HOUR, 2 * HOUR, &seeded, Some(0), Some(HALF)));
        assert!(is_due("r", HOUR, HOUR + HALF, &seeded, Some(0), None));
    }

    #[test]
    fn an_off_phase_last_fired_is_pulled_back_onto_the_phase() {
        let off = state(&[("r", HOUR + 900)]);
        assert!(!is_due("r", HOUR, HOUR + 960, &off, Some(HALF), Some(HALF)));
        assert!(is_due("r", HOUR, HOUR + HALF, &off, Some(HALF), Some(HALF)));
        assert!(!is_due("r", HOUR, HOUR + HALF, &off, None, None));
    }
}
