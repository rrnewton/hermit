//! Deterministic tick evaluation over injected gate and file-age boundaries.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io;
use std::thread;

use indexmap::IndexMap;

use crate::cadence::{
    is_due, unresolved_render_state_keys, UNRESOLVED_RENDER_COUNT_SUFFIX,
    UNRESOLVED_RENDER_FIRST_SUFFIX, UNRESOLVED_RENDER_STATE_PREFIX,
};
use crate::emit::{
    format_action, format_clean, format_error, format_health, format_no_result, format_note,
    format_suppressed, format_unevaluable, HEALTH_STATUS_MISSING, HEALTH_STATUS_OK,
    HEALTH_STATUS_STALE,
};
use crate::model::{Emit, EmitKind, Gate, GateWhen, HealthCheck, Reminder, TickConfig};
use crate::protocols::{FileAgeProbe, GateRunner};
use crate::state::{flag_truthy, state_lines, OpsState};
use crate::text::{split_lines, string_repr, trim};

const MAX_PARALLEL_GATES: usize = 8;

/// Everything produced by one tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TickResult {
    /// Ordered line protocol output.
    pub lines: Vec<String>,
    /// Advanced fired-state map.
    pub fired: BTreeMap<String, i64>,
    /// Number of `ACTION:` instructions emitted.
    pub actions_emitted: usize,
}

/// A rendered action or note still contains one or more template placeholders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedPlaceholderError {
    /// Sorted unique placeholders, including their braces.
    pub placeholders: Vec<String>,
}

impl fmt::Display for UnresolvedPlaceholderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "refusing emission with unresolved placeholder(s): {}",
            self.placeholders.join(", ")
        )
    }
}

impl Error for UnresolvedPlaceholderError {}

/// Parse non-comment `key=value` lines, retaining insertion order.
pub fn parse_kv_lines(text: &str) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for raw_line in split_lines(text) {
        let line = trim(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((raw_key, raw_value)) = line.split_once('=') {
            let key = trim(raw_key);
            if !key.is_empty() {
                out.insert(key.to_string(), trim(raw_value).to_string());
            }
        }
    }
    out
}

/// Evaluate one freshness check.
pub fn evaluate_health(hc: &HealthCheck, probe: &dyn FileAgeProbe, now: i64) -> String {
    let age = probe.newest_age_secs(&hc.glob, now);
    let status = match age {
        None => HEALTH_STATUS_MISSING,
        Some(age) if age <= hc.threshold_secs => HEALTH_STATUS_OK,
        Some(_) => HEALTH_STATUS_STALE,
    };
    format_health(&hc.name, status, age, hc.threshold_secs, &hc.detail)
}

/// Exit code reserved for "I could not determine my condition".
///
/// 75 is EX_TEMPFAIL ("temporary failure, user is invited to retry"), chosen on
/// collision evidence rather than taste: across the scripts behind the live
/// gate set the codes already in use are 0, 1, 2, 3, 124 and 127. 2 is the
/// tempting choice because one gate already prints NO_RESULT while exiting 2 --
/// and it is WRONG, because argparse exits 2 on a usage error, so reserving it
/// would render a crashed gate as a non-answer. Softening a real failure into
/// NO_RESULT is the opposite of the point.
pub const NO_RESULT_EXIT: i32 = 75;

/// The first consecutive unresolved-render count that adds a distinct escalation action.
pub const REPEATED_RENDER_FAILURE_THRESHOLD: i64 = 3;

/// What one gate execution concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// The gate determined its condition and it does not warrant emission.
    Quiet,
    /// The gate determined its condition and it warrants emission.
    Fire,
    /// The gate ran but could not determine its condition.
    NoResult,
}

struct ReminderEvaluation<'a> {
    reminder: &'a Reminder,
    outcome: GateOutcome,
    captured: IndexMap<String, String>,
    error: Option<String>,
}

enum PlannedReminder<'a> {
    Runnable(&'a Reminder),
    Suppressed(String),
}

enum ReminderReport<'a> {
    Evaluated(ReminderEvaluation<'a>),
    Suppressed(String),
}

type EmitLine<'a> = Option<&'a mut dyn FnMut(&str) -> io::Result<()>>;

fn gate_fires(when: GateWhen, returncode: i32, stdout: &str) -> bool {
    match when {
        GateWhen::Success => returncode == 0,
        GateWhen::Failure => returncode != 0,
        GateWhen::Nonempty => !trim(stdout).is_empty(),
        GateWhen::Always => true,
    }
}

fn eval_gate(
    gate: Option<&Gate>,
    runner: &dyn GateRunner,
) -> Result<(GateOutcome, IndexMap<String, String>), String> {
    let Some(gate) = gate else {
        return Ok((GateOutcome::Fire, IndexMap::new()));
    };
    let result = runner.run(
        &gate.cmd,
        gate.timeout_secs.and_then(|secs| u64::try_from(secs).ok()),
    );
    interpret_gate_result(gate, result)
}

fn interpret_gate_result(
    gate: &Gate,
    result: crate::protocols::GateResult,
) -> Result<(GateOutcome, IndexMap<String, String>), String> {
    if !result.ok {
        return Err(format!(
            "gate command could not run ({}): {}",
            string_repr(&gate.cmd),
            result.error.as_deref().unwrap_or("None")
        ));
    }
    let captured = if gate.capture {
        parse_kv_lines(&result.stdout)
    } else {
        IndexMap::new()
    };
    // Checked BEFORE `when`, so a gate that cannot determine its condition is
    // never reinterpreted through a fire/quiet rule. Under `when: failure` a
    // 75 would otherwise read as an ordinary failure verdict; under
    // `when: success` it would read as a clean pass, which is the exact
    // silence-means-healthy collapse this exists to remove.
    if result.returncode == NO_RESULT_EXIT {
        return Ok((GateOutcome::NoResult, captured));
    }
    let outcome = if gate_fires(gate.when, result.returncode, &result.stdout) {
        GateOutcome::Fire
    } else {
        GateOutcome::Quiet
    };
    Ok((outcome, captured))
}

fn placeholder_names(placeholders: &[String]) -> String {
    placeholders
        .iter()
        .map(|placeholder| {
            placeholder
                .strip_prefix('{')
                .and_then(|value| value.strip_suffix('}'))
                .unwrap_or(placeholder)
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn no_signal_action(
    reminder: &crate::model::Reminder,
    reason: &str,
    detail: &str,
    missing_placeholders: &[String],
) -> String {
    let mut fields = IndexMap::from([
        ("component".into(), "tick-hub-reporting".into()),
        ("outcome".into(), "NO-SIGNAL".into()),
        ("gate".into(), reminder.name.clone()),
        ("reason".into(), reason.into()),
    ]);
    let detail: String = detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect();
    if !detail.is_empty() {
        fields.insert("detail".into(), detail);
    }
    let mut title = format!("NO-SIGNAL gate={}: {reason}", reminder.name);
    if !missing_placeholders.is_empty() {
        let missing = placeholder_names(missing_placeholders);
        fields.insert("missing_placeholders".into(), missing.clone());
        title.push_str(&format!("; missing placeholder(s)={missing}"));
    }
    format_action(
        if reminder.emit.skill.is_empty() {
            "tick-hub-no-signal"
        } else {
            &reminder.emit.skill
        },
        &fields,
        &title,
    )
}

fn repeated_render_failure_action(
    reminder: &crate::model::Reminder,
    consecutive_failures: i64,
    first_failure_epoch: i64,
    missing_placeholders: &[String],
) -> String {
    let missing = placeholder_names(missing_placeholders);
    let fields = IndexMap::from([
        ("component".into(), "tick-hub-reporting".into()),
        ("outcome".into(), "NO-SIGNAL".into()),
        ("gate".into(), reminder.name.clone()),
        ("reason".into(), "unresolved-placeholder".into()),
        (
            "consecutive_failures".into(),
            consecutive_failures.to_string(),
        ),
        (
            "first_failure_epoch".into(),
            first_failure_epoch.to_string(),
        ),
        ("missing_placeholders".into(), missing.clone()),
    ]);
    let title = format!(
        "NO-SIGNAL gate={}: unresolved-placeholder repeated for {consecutive_failures} consecutive render failures since first_failure_epoch={first_failure_epoch}; missing placeholder(s)={missing}",
        reminder.name
    );
    format_action(
        if reminder.emit.skill.is_empty() {
            "tick-hub-no-signal"
        } else {
            &reminder.emit.skill
        },
        &fields,
        &title,
    )
}

fn interpolate(mut text: String, values: &IndexMap<String, String>) -> String {
    for (key, value) in values {
        text = text.replace(&format!("{{{key}}}"), value);
    }
    text
}

fn unresolved_placeholders(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut found = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'{' || (index > 0 && bytes[index - 1] == b'{') {
            index += 1;
            continue;
        }
        let first = index + 1;
        if first >= bytes.len() || !(bytes[first].is_ascii_alphabetic() || bytes[first] == b'_') {
            index += 1;
            continue;
        }
        let mut end = first + 1;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'.' | b'-'))
        {
            end += 1;
        }
        if end < bytes.len()
            && bytes[end] == b'}'
            && (end + 1 == bytes.len() || bytes[end + 1] != b'}')
        {
            found.insert(text[index..=end].to_string());
            index = end + 1;
        } else {
            index += 1;
        }
    }
    found
}

/// Render a fired emission, including captured-field interpolation.
pub fn render_emit(
    emit: &Emit,
    captured: &IndexMap<String, String>,
) -> Result<String, UnresolvedPlaceholderError> {
    let mut merged = emit.fields.clone();
    for (key, value) in captured {
        merged.insert(key.clone(), value.clone());
    }
    for key in emit.fields.keys() {
        if !captured.contains_key(key) {
            let value = merged.get(key).cloned().unwrap_or_default();
            merged.insert(key.clone(), interpolate(value, &merged));
        }
    }
    let title = interpolate(emit.title.clone(), &merged);
    let mut unresolved = unresolved_placeholders(&title);
    for value in merged.values() {
        unresolved.extend(unresolved_placeholders(value));
    }
    if !unresolved.is_empty() {
        return Err(UnresolvedPlaceholderError {
            placeholders: unresolved.into_iter().collect(),
        });
    }
    Ok(match emit.kind {
        EmitKind::Note => format_note(&title),
        EmitKind::Action => format_action(&emit.skill, &merged, &title),
    })
}

fn clear_render_failure_state(state: &mut BTreeMap<String, i64>, reminder_name: &str) {
    let (count_key, first_key) = unresolved_render_state_keys(reminder_name);
    state.remove(&count_key);
    state.remove(&first_key);
}

fn record_render_failure(
    state: &mut BTreeMap<String, i64>,
    reminder_name: &str,
    now: i64,
) -> (i64, i64) {
    let (count_key, first_key) = unresolved_render_state_keys(reminder_name);
    let previous_count = state.get(&count_key).copied().unwrap_or(0).max(0);
    let consecutive = previous_count.saturating_add(1);
    let first_failure_epoch = if previous_count == 0 {
        now.max(0)
    } else {
        state.get(&first_key).copied().unwrap_or(now.max(0)).max(0)
    };
    state.insert(count_key, consecutive);
    state.insert(first_key, first_failure_epoch);
    (consecutive, first_failure_epoch)
}

fn prune_removed_render_failure_state(
    state: &mut BTreeMap<String, i64>,
    reminder_names: &BTreeSet<&str>,
) {
    state.retain(|key, _| {
        let Some(rest) = key.strip_prefix(UNRESOLVED_RENDER_STATE_PREFIX) else {
            return true;
        };
        let name = rest
            .strip_suffix(UNRESOLVED_RENDER_COUNT_SUFFIX)
            .or_else(|| rest.strip_suffix(UNRESOLVED_RENDER_FIRST_SUFFIX));
        name.is_some_and(|name| !name.is_empty() && reminder_names.contains(name))
    });
}

fn record_line(lines: &mut Vec<String>, emit: &mut EmitLine<'_>, line: String) -> io::Result<()> {
    lines.push(line);
    if let Some(callback) = emit.as_deref_mut() {
        callback(lines.last().expect("line was just appended"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_evaluation(
    evaluation: &ReminderEvaluation<'_>,
    unavailable: &BTreeSet<String>,
    new_fired: &mut BTreeMap<String, i64>,
    now: i64,
    lines: &mut Vec<String>,
    emit: &mut EmitLine<'_>,
    report_pending: bool,
) -> io::Result<usize> {
    let reminder = evaluation.reminder;
    if let Some(error) = &evaluation.error {
        clear_render_failure_state(new_fired, &reminder.name);
        record_line(
            lines,
            emit,
            format_error(&format!("reminder {}: {error}", reminder.name)),
        )?;
        record_line(
            lines,
            emit,
            no_signal_action(reminder, "gate-execution-error", error, &[]),
        )?;
        return Ok(1);
    }
    if evaluation.outcome == GateOutcome::NoResult {
        clear_render_failure_state(new_fired, &reminder.name);
        let detail = evaluation
            .captured
            .get("summary")
            .map(String::as_str)
            .unwrap_or("");
        record_line(lines, emit, format_no_result(&reminder.name, detail))?;
        record_line(
            lines,
            emit,
            no_signal_action(reminder, "could-not-determine", detail, &[]),
        )?;
        return Ok(1);
    }
    if evaluation.outcome == GateOutcome::Quiet {
        clear_render_failure_state(new_fired, &reminder.name);
        let unavailable_dependencies: Vec<String> = reminder
            .depends_on
            .iter()
            .filter(|dependency| unavailable.contains(*dependency))
            .cloned()
            .collect();
        if !unavailable_dependencies.is_empty() {
            record_line(
                lines,
                emit,
                format_unevaluable(&reminder.name, &unavailable_dependencies),
            )?;
            record_line(
                lines,
                emit,
                no_signal_action(
                    reminder,
                    "dependency-could-not-determine",
                    &unavailable_dependencies.join(","),
                    &[],
                ),
            )?;
            return Ok(1);
        }
        if report_pending {
            record_line(lines, emit, format_clean(&reminder.name))?;
        }
        new_fired.insert(reminder.name.clone(), now);
        return Ok(0);
    }
    let line = match render_emit(&reminder.emit, &evaluation.captured) {
        Ok(line) => line,
        Err(error) => {
            record_line(
                lines,
                emit,
                format_error(&format!("reminder {}: {error}", reminder.name)),
            )?;
            record_line(
                lines,
                emit,
                no_signal_action(reminder, "unresolved-placeholder", "", &error.placeholders),
            )?;
            let mut actions = 1;
            let (consecutive, first_failure_epoch) =
                record_render_failure(new_fired, &reminder.name, now);
            if consecutive >= REPEATED_RENDER_FAILURE_THRESHOLD {
                record_line(
                    lines,
                    emit,
                    repeated_render_failure_action(
                        reminder,
                        consecutive,
                        first_failure_epoch,
                        &error.placeholders,
                    ),
                )?;
                actions += 1;
            }
            return Ok(actions);
        }
    };
    clear_render_failure_state(new_fired, &reminder.name);
    new_fired.insert(reminder.name.clone(), now);
    let actions = usize::from(line.starts_with("ACTION: "));
    record_line(lines, emit, line)?;
    Ok(actions)
}

#[allow(clippy::too_many_arguments)]
fn render_after_dependency_pass(
    reports: &[ReminderReport<'_>],
    render_from: usize,
    new_fired: &mut BTreeMap<String, i64>,
    now: i64,
    lines: &mut Vec<String>,
    emit: &mut EmitLine<'_>,
    report_pending: bool,
) -> io::Result<usize> {
    let mut unavailable: BTreeSet<String> = reports
        .iter()
        .filter_map(|report| match report {
            ReminderReport::Evaluated(evaluation)
                if evaluation.error.is_none() && evaluation.outcome == GateOutcome::NoResult =>
            {
                Some(evaluation.reminder.name.clone())
            }
            _ => None,
        })
        .collect();
    loop {
        let mut changed = false;
        for report in reports {
            let ReminderReport::Evaluated(evaluation) = report else {
                continue;
            };
            if evaluation.error.is_some()
                || evaluation.outcome != GateOutcome::Quiet
                || unavailable.contains(&evaluation.reminder.name)
            {
                continue;
            }
            if evaluation
                .reminder
                .depends_on
                .iter()
                .any(|dependency| unavailable.contains(dependency))
            {
                unavailable.insert(evaluation.reminder.name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut actions = 0;
    for report in &reports[render_from..] {
        match report {
            ReminderReport::Suppressed(line) => record_line(lines, emit, line.clone())?,
            ReminderReport::Evaluated(evaluation) => {
                actions += render_evaluation(
                    evaluation,
                    &unavailable,
                    new_fired,
                    now,
                    lines,
                    emit,
                    report_pending,
                )?;
            }
        }
    }
    Ok(actions)
}

/// Run one tick using explicit time and injected side-effect boundaries.
#[allow(clippy::too_many_arguments)]
pub fn run_tick(
    config: &TickConfig,
    state: &OpsState,
    now: i64,
    fired: &BTreeMap<String, i64>,
    gate_runner: &dyn GateRunner,
    age_probe: &dyn FileAgeProbe,
    current_tick_min: Option<i64>,
) -> TickResult {
    run_tick_inner(
        config,
        state,
        now,
        fired,
        gate_runner,
        age_probe,
        current_tick_min,
        false,
        None,
    )
    .expect("a tick without an emission callback cannot fail to write")
}

/// Run one tick while reporting each final line as soon as config order permits.
#[allow(clippy::too_many_arguments)]
pub fn run_tick_with_emit(
    config: &TickConfig,
    state: &OpsState,
    now: i64,
    fired: &BTreeMap<String, i64>,
    gate_runner: &dyn GateRunner,
    age_probe: &dyn FileAgeProbe,
    current_tick_min: Option<i64>,
    report_pending: bool,
    emit: &mut dyn FnMut(&str) -> io::Result<()>,
) -> io::Result<TickResult> {
    run_tick_inner(
        config,
        state,
        now,
        fired,
        gate_runner,
        age_probe,
        current_tick_min,
        report_pending,
        Some(emit),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_tick_inner(
    config: &TickConfig,
    state: &OpsState,
    now: i64,
    fired: &BTreeMap<String, i64>,
    gate_runner: &dyn GateRunner,
    age_probe: &dyn FileAgeProbe,
    current_tick_min: Option<i64>,
    report_pending: bool,
    mut emit: EmitLine<'_>,
) -> io::Result<TickResult> {
    let mut lines = Vec::new();
    let mut actions = 0;
    for health in &config.health_checks {
        record_line(
            &mut lines,
            &mut emit,
            evaluate_health(health, age_probe, now),
        )?;
    }
    for line in state_lines(state, current_tick_min) {
        if line.starts_with("ACTION: ") {
            actions += 1;
        }
        record_line(&mut lines, &mut emit, line)?;
    }
    let mut new_fired = fired.clone();
    let reminder_names = config
        .reminders
        .iter()
        .map(|reminder| reminder.name.as_str())
        .collect::<BTreeSet<_>>();
    prune_removed_render_failure_state(&mut new_fired, &reminder_names);
    if state.enabled {
        let mut planned = Vec::new();
        for reminder in &config.reminders {
            if !is_due(
                &reminder.name,
                reminder.cadence_secs,
                now,
                fired,
                reminder.cadence_offset_secs,
                reminder.cadence_window_secs,
            ) {
                continue;
            }
            if !reminder
                .requires_flags
                .iter()
                .all(|name| flag_truthy(&state.flags, name))
            {
                if report_pending {
                    let missing = reminder
                        .requires_flags
                        .iter()
                        .filter(|name| !flag_truthy(&state.flags, name))
                        .cloned()
                        .collect::<Vec<_>>();
                    planned.push(PlannedReminder::Suppressed(format_suppressed(
                        &reminder.name,
                        &missing,
                    )));
                }
                continue;
            }
            planned.push(PlannedReminder::Runnable(reminder));
        }

        let stream_prefix_len = if emit.is_some() {
            planned
                .iter()
                .position(|entry| {
                    matches!(entry, PlannedReminder::Runnable(reminder) if !reminder.depends_on.is_empty())
                })
                .unwrap_or(planned.len())
        } else {
            0
        };
        let no_dependencies = BTreeSet::new();
        let mut reports = Vec::new();
        let parallel_indexes = planned
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry {
                PlannedReminder::Runnable(reminder)
                    if reminder.gate.as_ref().is_some_and(|gate| gate.parallel) =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        thread::scope(|scope| -> io::Result<()> {
            let mut parallel = BTreeMap::new();
            if parallel_indexes.len() >= 2 {
                for index in parallel_indexes.iter().take(MAX_PARALLEL_GATES) {
                    let PlannedReminder::Runnable(reminder) = &planned[*index] else {
                        unreachable!("parallel indexes contain runnable reminders")
                    };
                    let reminder = *reminder;
                    parallel.insert(
                        *index,
                        scope.spawn(move || eval_gate(reminder.gate.as_ref(), gate_runner)),
                    );
                }
            }
            let mut next_parallel = MAX_PARALLEL_GATES;
            for (index, entry) in planned.iter().enumerate() {
                let report = match entry {
                    PlannedReminder::Suppressed(line) => ReminderReport::Suppressed(line.clone()),
                    PlannedReminder::Runnable(reminder) => {
                        let evaluated = match parallel.remove(&index) {
                            Some(handle) => {
                                let result = handle.join().unwrap_or_else(|_| {
                                    Err("parallel gate worker panicked".into())
                                });
                                if let Some(next_index) = parallel_indexes.get(next_parallel) {
                                    let PlannedReminder::Runnable(next_reminder) =
                                        &planned[*next_index]
                                    else {
                                        unreachable!("parallel indexes contain runnable reminders")
                                    };
                                    let next_reminder = *next_reminder;
                                    parallel.insert(
                                        *next_index,
                                        scope.spawn(move || {
                                            eval_gate(next_reminder.gate.as_ref(), gate_runner)
                                        }),
                                    );
                                    next_parallel += 1;
                                }
                                result
                            }
                            None => eval_gate(reminder.gate.as_ref(), gate_runner),
                        };
                        let (outcome, captured, error) = match evaluated {
                            Ok((outcome, captured)) => (outcome, captured, None),
                            Err(error) => (GateOutcome::Quiet, IndexMap::new(), Some(error)),
                        };
                        ReminderReport::Evaluated(ReminderEvaluation {
                            reminder,
                            outcome,
                            captured,
                            error,
                        })
                    }
                };
                reports.push(report);
                if index < stream_prefix_len {
                    match reports.last().expect("report was just appended") {
                        ReminderReport::Suppressed(line) => {
                            record_line(&mut lines, &mut emit, line.clone())?;
                        }
                        ReminderReport::Evaluated(evaluation) => {
                            actions += render_evaluation(
                                evaluation,
                                &no_dependencies,
                                &mut new_fired,
                                now,
                                &mut lines,
                                &mut emit,
                                report_pending,
                            )?;
                        }
                    }
                }
            }
            Ok(())
        })?;
        if stream_prefix_len < reports.len() {
            actions += render_after_dependency_pass(
                &reports,
                stream_prefix_len,
                &mut new_fired,
                now,
                &mut lines,
                &mut emit,
                report_pending,
            )?;
        }
    }
    record_line(
        &mut lines,
        &mut emit,
        format_note(&format!("emitted {actions} instruction(s) this tick")),
    )?;
    Ok(TickResult {
        lines,
        fired: new_fired,
        actions_emitted: actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Emit, Gate, HealthCheck, Reminder};
    use crate::protocols::GateResult;
    use crate::state::FlagValue;
    use std::sync::{Arc, Mutex};

    struct FakeGate {
        outcomes: BTreeMap<String, GateResult>,
        calls: Mutex<Vec<String>>,
    }

    impl GateRunner for FakeGate {
        fn run(&self, cmd: &str, _timeout_secs: Option<u64>) -> GateResult {
            self.calls.lock().unwrap().push(cmd.to_string());
            self.outcomes
                .get(cmd)
                .cloned()
                .unwrap_or_else(|| GateResult::completed(0, ""))
        }
    }

    struct FakeProbe(BTreeMap<String, Option<i64>>);

    impl FileAgeProbe for FakeProbe {
        fn newest_age_secs(&self, pattern: &str, _now: i64) -> Option<i64> {
            self.0.get(pattern).copied().flatten()
        }
    }

    fn fakes() -> (FakeGate, FakeProbe) {
        (
            FakeGate {
                outcomes: BTreeMap::new(),
                calls: Mutex::new(Vec::new()),
            },
            FakeProbe(BTreeMap::new()),
        )
    }

    #[test]
    fn health_and_trailing_summary_are_ordered() {
        let config = TickConfig {
            health_checks: vec![
                HealthCheck {
                    name: "fresh".into(),
                    glob: "/f".into(),
                    threshold_secs: 100,
                    detail: "f".into(),
                },
                HealthCheck {
                    name: "gone".into(),
                    glob: "/g".into(),
                    threshold_secs: 100,
                    detail: "g".into(),
                },
            ],
            ..TickConfig::default()
        };
        let (gate, _) = fakes();
        let probe = FakeProbe(BTreeMap::from([
            ("/f".into(), Some(10)),
            ("/g".into(), None),
        ]));
        let result = run_tick(
            &config,
            &OpsState::default(),
            0,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(result.lines[0].starts_with("HEALTH: fresh ok"));
        assert!(result.lines[1].starts_with("HEALTH: gone missing"));
        assert_eq!(
            result.lines.last().unwrap(),
            "NOTE: emitted 0 instruction(s) this tick"
        );
    }

    #[test]
    fn explicitly_parallel_gates_overlap_and_keep_config_order() {
        let marker = std::env::temp_dir().join(format!(
            "tick-hub-parallel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut first = Reminder::new("first", Emit::action("warn", "first"));
        let mut first_gate = Gate::new(format!(
            "for unused in $(seq 1 200); do [ -e {} ] && exit 1; sleep 0.01; done; exit 75",
            marker.display()
        ));
        first_gate.when = GateWhen::Failure;
        first_gate.parallel = true;
        first.gate = Some(first_gate);

        let mut second = Reminder::new("second", Emit::action("warn", "second"));
        let mut second_gate = Gate::new(format!("sleep 0.1; : > {}; exit 1", marker.display()));
        second_gate.when = GateWhen::Failure;
        second_gate.parallel = true;
        second.gate = Some(second_gate);

        let result = run_tick(
            &TickConfig {
                reminders: vec![first, second],
                ..TickConfig::default()
            },
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &crate::probes::SubprocessGateRunner::new(4),
            &FakeProbe(BTreeMap::new()),
            None,
        );
        let actions = result
            .lines
            .iter()
            .filter(|line| line.starts_with("ACTION: warn"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                "ACTION: warn title=\"first\"".to_string(),
                "ACTION: warn title=\"second\"".to_string()
            ]
        );
        assert_eq!(
            result.fired,
            BTreeMap::from([("first".into(), 5), ("second".into(), 5)])
        );
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn parallel_prefix_is_emitted_while_a_later_gate_is_running() {
        let root = std::env::temp_dir().join(format!(
            "tick-hub-parallel-stream-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let started = root.join("second-started");
        let release = root.join("release-second");
        let finished = root.join("second-finished");

        let mut first = Reminder::new("first", Emit::action("warn", "first"));
        let mut first_gate = Gate::new(format!(
            "for unused in $(seq 1 200); do [ -e {} ] && exit 1; sleep 0.01; done; exit 75",
            started.display()
        ));
        first_gate.when = GateWhen::Failure;
        first_gate.parallel = true;
        first.gate = Some(first_gate);

        let mut second = Reminder::new("second", Emit::action("warn", "second"));
        let mut second_gate = Gate::new(format!(
            ": > {}; while [ ! -e {} ]; do sleep 0.01; done; : > {}",
            started.display(),
            release.display(),
            finished.display()
        ));
        second_gate.when = GateWhen::Failure;
        second_gate.parallel = true;
        second.gate = Some(second_gate);

        let config = TickConfig {
            reminders: vec![first, second],
            ..TickConfig::default()
        };
        let mut observed_running = false;
        let mut emit = |line: &str| {
            if line.starts_with("ACTION: warn") {
                observed_running = started.exists() && !finished.exists();
                std::fs::write(&release, b"release").unwrap();
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"));
            }
            Ok(())
        };
        let result = run_tick_with_emit(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &crate::probes::SubprocessGateRunner::new(4),
            &FakeProbe(BTreeMap::new()),
            None,
            false,
            &mut emit,
        );
        assert!(result.is_err());
        assert!(
            observed_running,
            "the first verdict waited for the later gate"
        );
        assert!(
            finished.exists(),
            "the worker was not joined after write failure"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_interpolates_and_preserves_field_order() {
        let mut emit = Emit::action("triage", "{count} ready (> {threshold})");
        emit.fields.insert("threshold".into(), "5".into());
        let mut reminder = Reminder::new("backlog", emit);
        reminder.gate = Some(Gate {
            cmd: "count".into(),
            when: GateWhen::Always,
            capture: true,
            parallel: false,
            timeout_secs: None,
        });
        let config = TickConfig {
            reminders: vec![reminder],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([("count".into(), GateResult::completed(0, "count=7\n"))]),
            calls: Mutex::new(Vec::new()),
        };
        let probe = FakeProbe(BTreeMap::new());
        let result = run_tick(
            &config,
            &OpsState::default(),
            0,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(result
            .lines
            .contains(&"ACTION: triage threshold=5 count=7 title=\"7 ready (> 5)\"".to_string()));
        assert_eq!(result.fired.get("backlog"), Some(&0));
    }

    #[test]
    fn static_fields_can_interpolate_other_static_and_captured_fields() {
        let mut emit = Emit::action("triage", "{copy}/{live}");
        emit.fields.insert("base".into(), "7".into());
        emit.fields.insert("copy".into(), "{base}".into());
        emit.fields.insert("live".into(), "{count}".into());
        let captured = IndexMap::from([("count".into(), "3".into())]);
        assert_eq!(
            render_emit(&emit, &captured).unwrap(),
            "ACTION: triage base=7 copy=7 live=3 count=3 title=\"7/3\""
        );
    }

    #[test]
    fn captured_values_honor_all_line_boundaries() {
        assert_eq!(
            parse_kv_lines("a=1\rb=2\u{b}c=3\u{85}d=4\u{2028}e=5"),
            IndexMap::<String, String>::from([
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
                ("c".into(), "3".into()),
                ("d".into(), "4".into()),
                ("e".into(), "5".into()),
            ])
        );
    }

    #[test]
    fn flag_suppression_does_not_consume_cadence() {
        let mut reminder = Reminder::new("bench", Emit::action("run-benchmark", "refresh"));
        reminder.requires_flags.push("benchmark_enabled".into());
        let config = TickConfig {
            reminders: vec![reminder],
            ..TickConfig::default()
        };
        let state = OpsState {
            flags: BTreeMap::from([("benchmark_enabled".into(), FlagValue::Bool(false))]),
            ..OpsState::default()
        };
        let (gate, probe) = fakes();
        let result = run_tick(&config, &state, 10, &BTreeMap::new(), &gate, &probe, None);
        assert!(!result
            .lines
            .iter()
            .any(|line| line.contains("run-benchmark")));
        assert!(!result.fired.contains_key("bench"));
    }

    #[test]
    fn gate_execution_failure_is_retried() {
        let mut reminder = Reminder::new("x", Emit::action("x", "x"));
        reminder.gate = Some(Gate::new("boom"));
        let config = TickConfig {
            reminders: vec![reminder],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([("boom".into(), GateResult::failed("not found"))]),
            calls: Mutex::new(Vec::new()),
        };
        let probe = FakeProbe(BTreeMap::new());
        let result = run_tick(
            &config,
            &OpsState::default(),
            9,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(result.lines.iter().any(|line| {
            line == "ERROR: reminder x: gate command could not run ('boom'): not found"
        }));
        assert!(result
            .lines
            .iter()
            .any(|line| line.contains("reason=gate-execution-error")));
        assert_eq!(result.actions_emitted, 1);
        assert!(!result.fired.contains_key("x"));
    }

    // --- COULD-NOT-DETERMINE, BOTH DIRECTIONS ---------------------------------
    //
    // The negative half is the point: a gate that cannot determine its
    // condition must be visibly distinct from one that checked and found
    // nothing. The positive halves prove the change is not inert -- a gate that
    // CAN determine still reports exactly what it reported before.

    fn gated(
        name: &str,
        cmd: &str,
        when: GateWhen,
        code: i32,
        stdout: &str,
    ) -> (TickConfig, FakeGate, FakeProbe) {
        let mut reminder = Reminder::new(name, Emit::action("warn", "{summary}"));
        let mut gate = Gate::new(cmd);
        gate.when = when;
        gate.capture = true;
        reminder.gate = Some(gate);
        let config = TickConfig {
            reminders: vec![reminder],
            ..TickConfig::default()
        };
        let runner = FakeGate {
            outcomes: BTreeMap::from([(cmd.into(), GateResult::completed(code, stdout))]),
            calls: Mutex::new(Vec::new()),
        };
        (config, runner, FakeProbe(BTreeMap::new()))
    }

    #[test]
    fn unresolved_placeholder_is_refused_loudly_and_retried() {
        let (config, gate, probe) = gated("obligation", "check", GateWhen::Failure, 1, "");
        let result = run_tick(
            &config,
            &OpsState::default(),
            7,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        let actions = result
            .lines
            .iter()
            .filter(|line| line.starts_with("ACTION: "))
            .collect::<Vec<_>>();
        assert_eq!(actions.len(), 1, "{:?}", result.lines);
        assert!(actions[0].contains("outcome=NO-SIGNAL"));
        assert!(actions[0].contains("reason=unresolved-placeholder"));
        assert!(actions[0].contains("missing_placeholders=summary"));
        assert!(result.lines.iter().any(|line| {
            line == "ERROR: reminder obligation: refusing emission with unresolved placeholder(s): {summary}"
        }));
        let (count_key, first_key) = unresolved_render_state_keys("obligation");
        assert_eq!(result.fired.get(&count_key), Some(&1));
        assert_eq!(result.fired.get(&first_key), Some(&7));
        assert!(!result.fired.contains_key("obligation"));
        assert_eq!(result.actions_emitted, 1);
    }

    #[test]
    fn third_consecutive_unresolved_placeholder_adds_persistent_escalation() {
        // Mutation controls: >=3 -> >3 misses the third tick; resetting the first epoch on each
        // failure changes the asserted first_failure_epoch=100.
        let (config, gate, probe) = gated("obligation", "check", GateWhen::Failure, 1, "");
        let (count_key, first_key) = unresolved_render_state_keys("obligation");
        let mut fired = BTreeMap::new();
        for (index, now) in [100, 200, 300].into_iter().enumerate() {
            let consecutive = i64::try_from(index + 1).unwrap();
            let result = run_tick(
                &config,
                &OpsState::default(),
                now,
                &fired,
                &gate,
                &probe,
                None,
            );
            let actions = result
                .lines
                .iter()
                .filter(|line| line.starts_with("ACTION: "))
                .collect::<Vec<_>>();
            assert!(actions
                .iter()
                .any(|line| line.contains("reason=unresolved-placeholder")));
            assert_eq!(actions.len(), if consecutive < 3 { 1 } else { 2 });
            assert_eq!(result.actions_emitted, actions.len());
            if consecutive < 3 {
                assert!(!actions
                    .iter()
                    .any(|line| line.contains("consecutive_failures=")));
            } else {
                let repeated = actions
                    .iter()
                    .find(|line| line.contains("consecutive_failures="))
                    .expect("third failure escalation");
                assert!(repeated.contains("consecutive_failures=3"));
                assert!(repeated.contains("first_failure_epoch=100"));
                assert!(repeated.contains("missing_placeholders=summary"));
            }
            assert_eq!(result.fired.get(&count_key), Some(&consecutive));
            assert_eq!(result.fired.get(&first_key), Some(&100));
            assert!(!result.fired.contains_key("obligation"));
            fired = result.fired;
        }
    }

    #[test]
    fn any_later_non_render_failure_outcome_clears_render_failure_state() {
        // Mutation control: deleting any branch's clear call leaves one seeded key live.
        let (config, _, probe) = gated("obligation", "check", GateWhen::Failure, 1, "");
        let (count_key, first_key) = unresolved_render_state_keys("obligation");
        let prior = BTreeMap::from([(count_key.clone(), 4), (first_key.clone(), 10)]);
        for outcome in [
            GateResult::failed("not found"),
            GateResult::completed(NO_RESULT_EXIT, ""),
            GateResult::completed(0, ""),
            GateResult::completed(1, "summary=rendered\n"),
        ] {
            let gate = FakeGate {
                outcomes: BTreeMap::from([("check".into(), outcome)]),
                calls: Mutex::new(Vec::new()),
            };
            let result = run_tick(
                &config,
                &OpsState::default(),
                20,
                &prior,
                &gate,
                &probe,
                None,
            );
            assert!(!result.fired.contains_key(&count_key));
            assert!(!result.fired.contains_key(&first_key));
        }
    }

    #[test]
    fn removed_reminder_render_failure_state_is_pruned_without_touching_cadence() {
        let (count_key, first_key) = unresolved_render_state_keys("removed");
        let fired = BTreeMap::from([
            ("still-config-independent".into(), 7),
            (count_key, 4),
            (first_key, 10),
        ]);
        let (gate, probe) = fakes();
        let result = run_tick(
            &TickConfig::default(),
            &OpsState::default(),
            20,
            &fired,
            &gate,
            &probe,
            None,
        );
        assert_eq!(
            result.fired,
            BTreeMap::from([("still-config-independent".into(), 7)])
        );
    }

    #[test]
    fn no_evaluation_keeps_active_reminder_render_failure_state() {
        let reminder = Reminder::new("obligation", Emit::note("{summary}"));
        let config = TickConfig {
            reminders: vec![reminder],
            ..TickConfig::default()
        };
        let (count_key, first_key) = unresolved_render_state_keys("obligation");
        let prior = BTreeMap::from([(count_key, 2), (first_key, 10)]);
        let (gate, probe) = fakes();
        let state = OpsState {
            enabled: false,
            ..OpsState::default()
        };
        let result = run_tick(&config, &state, 20, &prior, &gate, &probe, None);
        assert_eq!(result.fired, prior);
    }

    #[test]
    fn could_not_determine_renders_no_result_not_a_pass() {
        let (config, gate, probe) = gated(
            "watcher",
            "probe",
            GateWhen::Failure,
            NO_RESULT_EXIT,
            "summary=backend unreachable\n",
        );
        let result = run_tick(
            &config,
            &OpsState::default(),
            10,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        let line = result
            .lines
            .iter()
            .find(|l| l.starts_with("NO_RESULT: "))
            .expect("a NO_RESULT line");
        assert!(line.contains("watcher"), "names the gate: {line}");
        assert!(
            line.contains("this is not a pass"),
            "distinct from a pass: {line}"
        );
        assert!(
            line.contains("backend unreachable"),
            "carries detail when present: {line}"
        );
        assert!(result
            .lines
            .iter()
            .any(|line| line.contains("reason=could-not-determine")));
        assert!(!result.lines.iter().any(|line| line.contains("{summary}")));
    }

    #[test]
    fn no_result_is_emittable_when_the_gate_printed_nothing() {
        // THE CORRELATED-FAILURE CASE. A gate that cannot determine its
        // condition is the one least likely to produce a usable summary=, so
        // the line must not depend on captured fields or on emit.title.
        let (config, gate, probe) = gated("mute", "probe", GateWhen::Failure, NO_RESULT_EXIT, "");
        let result = run_tick(
            &config,
            &OpsState::default(),
            10,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        let line = result
            .lines
            .iter()
            .find(|l| l.starts_with("NO_RESULT: "))
            .expect("a NO_RESULT line");
        assert!(line.contains("mute"));
        assert!(
            !line.contains('{'),
            "must not leak an unresolved placeholder: {line}"
        );
    }

    #[test]
    fn no_result_does_not_consume_cadence() {
        let (config, gate, probe) = gated("retry", "probe", GateWhen::Failure, NO_RESULT_EXIT, "");
        let result = run_tick(
            &config,
            &OpsState::default(),
            10,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(
            !result.fired.contains_key("retry"),
            "must re-announce next tick"
        );
    }

    #[test]
    fn no_result_is_not_reinterpreted_by_when_success() {
        // Under `when: success` a nonzero code means "quiet". Without the
        // pre-check, 75 would silently read as a clean pass -- the exact
        // collapse this change removes.
        let (config, gate, probe) = gated("s", "probe", GateWhen::Success, NO_RESULT_EXIT, "");
        let result = run_tick(
            &config,
            &OpsState::default(),
            10,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(
            result.lines.iter().any(|l| l.starts_with("NO_RESULT: ")),
            "{:?}",
            result.lines
        );
    }

    #[test]
    fn positive_control_a_determinable_gate_is_unchanged() {
        // fires on real failure ...
        let (config, gate, probe) =
            gated("f", "probe", GateWhen::Failure, 1, "summary=real problem\n");
        let fired = run_tick(
            &config,
            &OpsState::default(),
            10,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(fired
            .lines
            .iter()
            .any(|l| l.starts_with("ACTION: ") && l.contains("real problem")));
        assert!(!fired.lines.iter().any(|l| l.starts_with("NO_RESULT: ")));
        assert!(
            fired.fired.contains_key("f"),
            "a real verdict still consumes cadence"
        );
        // ... and stays quiet on a clean check.
        let (config, gate, probe) = gated("q", "probe", GateWhen::Failure, 0, "");
        let quiet = run_tick(
            &config,
            &OpsState::default(),
            10,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
        );
        assert!(!quiet
            .lines
            .iter()
            .any(|l| l.starts_with("ACTION: ") || l.starts_with("NO_RESULT: ")));
        assert!(quiet.fired.contains_key("q"));
    }

    fn dependency_probe(name: &str, dependencies: &[&str]) -> Reminder {
        let mut reminder = Reminder::new(
            name,
            Emit::action("warn", format!("{name} found a problem")),
        );
        let mut gate = Gate::new(name);
        gate.when = GateWhen::Failure;
        reminder.gate = Some(gate);
        reminder.depends_on = dependencies
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        reminder
    }

    #[test]
    fn foundation_no_result_marks_quiet_dependents_unevaluable() {
        let config = TickConfig {
            reminders: vec![
                dependency_probe("dependent", &["foundation"]),
                dependency_probe("independent", &[]),
                dependency_probe("foundation", &[]),
            ],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([
                (
                    "foundation".into(),
                    GateResult::completed(NO_RESULT_EXIT, ""),
                ),
                ("dependent".into(), GateResult::completed(0, "")),
                ("independent".into(), GateResult::completed(0, "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let result = run_tick(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &gate,
            &FakeProbe(BTreeMap::new()),
            None,
        );
        assert_eq!(
            *gate.calls.lock().unwrap(),
            vec!["dependent", "independent", "foundation"]
        );
        assert!(result.lines.iter().any(|line| line
            .starts_with("NO_RESULT: dependent is unevaluable because dependency foundation")));
        assert!(!result.fired.contains_key("dependent"));
        assert!(!result.fired.contains_key("foundation"));
        assert_eq!(result.fired.get("independent"), Some(&5));
    }

    #[test]
    fn dependency_never_suppresses_a_real_finding() {
        let config = TickConfig {
            reminders: vec![
                dependency_probe("foundation", &[]),
                dependency_probe("dependent", &["foundation"]),
            ],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([
                (
                    "foundation".into(),
                    GateResult::completed(NO_RESULT_EXIT, ""),
                ),
                ("dependent".into(), GateResult::completed(1, "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let result = run_tick(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &gate,
            &FakeProbe(BTreeMap::new()),
            None,
        );
        assert!(result
            .lines
            .iter()
            .any(|line| line.contains("dependent found a problem")));
        assert!(!result
            .lines
            .iter()
            .any(|line| line.contains("dependent is unevaluable")));
        assert_eq!(result.fired.get("dependent"), Some(&5));
    }

    #[test]
    fn dependency_propagates_through_quiet_chain_only() {
        let config = TickConfig {
            reminders: vec![
                dependency_probe("foundation", &[]),
                dependency_probe("middle", &["foundation"]),
                dependency_probe("leaf", &["middle"]),
            ],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([
                (
                    "foundation".into(),
                    GateResult::completed(NO_RESULT_EXIT, ""),
                ),
                ("middle".into(), GateResult::completed(0, "")),
                ("leaf".into(), GateResult::completed(0, "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let result = run_tick(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &gate,
            &FakeProbe(BTreeMap::new()),
            None,
        );
        assert!(result
            .lines
            .iter()
            .any(|line| line.contains("middle is unevaluable")));
        assert!(result
            .lines
            .iter()
            .any(|line| line.contains("leaf is unevaluable")));
    }

    #[test]
    fn empty_success_is_not_inferred_to_be_no_result() {
        let config = TickConfig {
            reminders: vec![
                dependency_probe("foundation", &[]),
                dependency_probe("dependent", &["foundation"]),
            ],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([
                ("foundation".into(), GateResult::completed(0, "")),
                ("dependent".into(), GateResult::completed(0, "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let result = run_tick(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &gate,
            &FakeProbe(BTreeMap::new()),
            None,
        );
        assert!(!result
            .lines
            .iter()
            .any(|line| line.starts_with("NO_RESULT: ")));
        assert_eq!(
            result.fired,
            BTreeMap::from([("foundation".into(), 5), ("dependent".into(), 5)])
        );
    }

    struct RecordingGate {
        outcomes: BTreeMap<String, GateResult>,
        emitted: Arc<Mutex<Vec<String>>>,
        seen_at_call: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl GateRunner for RecordingGate {
        fn run(&self, cmd: &str, _timeout_secs: Option<u64>) -> GateResult {
            self.seen_at_call
                .lock()
                .unwrap()
                .push((cmd.to_string(), self.emitted.lock().unwrap().clone()));
            self.outcomes
                .get(cmd)
                .cloned()
                .unwrap_or_else(|| GateResult::completed(0, ""))
        }
    }

    #[test]
    fn independent_prefix_streams_before_a_later_dependency_group() {
        let config = TickConfig {
            reminders: vec![
                dependency_probe("independent", &[]),
                dependency_probe("dependent", &["foundation"]),
                dependency_probe("foundation", &[]),
            ],
            ..TickConfig::default()
        };
        let outcomes = BTreeMap::from([
            ("independent".into(), GateResult::completed(1, "")),
            ("dependent".into(), GateResult::completed(0, "")),
            (
                "foundation".into(),
                GateResult::completed(NO_RESULT_EXIT, ""),
            ),
        ]);
        let collected = run_tick_inner(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &FakeGate {
                outcomes: outcomes.clone(),
                calls: Mutex::new(Vec::new()),
            },
            &FakeProbe(BTreeMap::new()),
            None,
            true,
            None,
        )
        .unwrap();
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingGate {
            outcomes,
            emitted: Arc::clone(&emitted),
            seen_at_call: Mutex::new(Vec::new()),
        };
        let emitted_for_callback = Arc::clone(&emitted);
        let mut callback = move |line: &str| {
            emitted_for_callback.lock().unwrap().push(line.into());
            Ok(())
        };
        let streamed = run_tick_with_emit(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &runner,
            &FakeProbe(BTreeMap::new()),
            None,
            true,
            &mut callback,
        )
        .unwrap();

        let seen = runner.seen_at_call.lock().unwrap();
        let before_dependent = &seen.iter().find(|(cmd, _)| cmd == "dependent").unwrap().1;
        assert!(before_dependent
            .iter()
            .any(|line| line.contains("independent found a problem")));
        let before_foundation = &seen.iter().find(|(cmd, _)| cmd == "foundation").unwrap().1;
        assert!(!before_foundation
            .iter()
            .any(|line| line == "CLEAN: dependent ran and found nothing to report"));
        assert_eq!(*emitted.lock().unwrap(), collected.lines);
        assert_eq!(streamed, collected);
    }

    #[test]
    fn suppressed_verdict_keeps_config_order_between_runnable_gates() {
        let mut suppressed = Reminder::new("suppressed", Emit::action("warn", "suppressed"));
        suppressed.requires_flags.push("enabled".into());
        let config = TickConfig {
            reminders: vec![
                dependency_probe("first", &[]),
                suppressed,
                dependency_probe("third", &[]),
            ],
            ..TickConfig::default()
        };
        let gate = FakeGate {
            outcomes: BTreeMap::from([
                ("first".into(), GateResult::completed(1, "")),
                ("third".into(), GateResult::completed(1, "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let mut emitted = Vec::new();
        let result = run_tick_with_emit(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &gate,
            &FakeProbe(BTreeMap::new()),
            None,
            true,
            &mut |line| {
                emitted.push(line.to_string());
                Ok(())
            },
        )
        .unwrap();
        let verdicts = result
            .lines
            .iter()
            .filter(|line| line.starts_with("ACTION: warn") || line.starts_with("SUPPRESSED: "))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            verdicts,
            vec![
                "ACTION: warn title=\"first found a problem\"",
                "SUPPRESSED: suppressed did not run; required flag(s) not set: enabled",
                "ACTION: warn title=\"third found a problem\"",
            ]
        );
        assert_eq!(emitted, result.lines);
    }

    #[test]
    fn emission_failure_stops_the_tick() {
        let config = TickConfig {
            reminders: vec![dependency_probe("first", &[])],
            ..TickConfig::default()
        };
        let (gate, probe) = fakes();
        let error = run_tick_with_emit(
            &config,
            &OpsState::default(),
            5,
            &BTreeMap::new(),
            &gate,
            &probe,
            None,
            true,
            &mut |_| Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed")),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(
            gate.calls.lock().unwrap().is_empty(),
            "evaluation must stop at the failed write"
        );
    }
}
