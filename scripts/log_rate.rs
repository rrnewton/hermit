#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Analyze completed-syscall throughput in Hermit logs.
//!
//! Usage:
//!   ./scripts/log_rate.rs < /tmp/qemu-hermit.log
//!   ./scripts/log_rate.rs --window 1 --step 0.5 /tmp/qemu-hermit.log
//!   ./scripts/log_rate.rs --drops-only /tmp/qemu-hermit.log

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::env;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::io::{self};
use std::path::PathBuf;
use std::process;

const FINISH_MARKER: &str = "finish syscall #";
const DTID_MARKER: &str = "[detcore, dtid ";
const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Clone, Debug)]
struct Config {
    window_ns: i128,
    step_ns: i128,
    drop_ratio: f64,
    baseline_windows: usize,
    min_baseline_count: usize,
    drops_only: bool,
    input: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            window_ns: NANOS_PER_SECOND,
            step_ns: NANOS_PER_SECOND / 2,
            drop_ratio: 0.5,
            baseline_windows: 5,
            min_baseline_count: 5,
            drops_only: false,
            input: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Event {
    timestamp_ns: i128,
    dtid: u64,
    syscall_number: u64,
}

#[derive(Debug)]
struct ParsedLog {
    events: Vec<Event>,
    first_event_ns: i128,
    last_timestamp_ns: i128,
    lines: usize,
    timestamped_lines: usize,
    malformed_completions: usize,
}

#[derive(Clone, Debug)]
struct Row {
    start_ns: i128,
    end_ns: i128,
    dtid: Option<u64>,
    count: usize,
    rate: f64,
    baseline: Option<f64>,
    drop: bool,
    first_syscall: Option<u64>,
    last_syscall: Option<u64>,
}

fn usage() -> &'static str {
    "Usage: log_rate.rs [OPTIONS] [LOG_FILE|-]\n\
\n\
Reads Hermit info/debug/trace output and counts completed syscalls once.\n\
With no LOG_FILE, input is read from stdin.\n\
\n\
Options:\n\
  --window SECONDS          Sliding-window width (default: 1.0)\n\
  --step SECONDS            Distance between windows (default: 0.5)\n\
  --drop-ratio RATIO        Mark rates at or below this baseline ratio\n\
                            as drops (default: 0.5)\n\
  --baseline-windows N      Prior windows in rolling median (default: 5)\n\
  --min-baseline-count N    Ignore baselines below N syscalls/window\n\
                            (default: 5)\n\
  --drops-only              Print only rows classified as drops\n\
  -h, --help                Show this help\n"
}

fn main() {
    if let Err(error) = run() {
        eprintln!("log_rate: {error}");
        process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let config = parse_args(env::args().skip(1))?;
    let source = config
        .input
        .as_ref()
        .map_or_else(|| "stdin".to_owned(), |path| path.display().to_string());

    let parsed = if let Some(path) = &config.input {
        let file =
            File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        parse_log(BufReader::new(file))?
    } else {
        parse_log(BufReader::new(io::stdin().lock()))?
    };

    let rows = analyze(&parsed, &config);
    let output = render(&source, &parsed, &config, &rows);
    match io::stdout().write_all(output.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("cannot write output: {error}")),
    }
}

fn parse_args<I>(args: I) -> Result<Config, String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut config = Config::default();
    let mut index = 0;

    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                process::exit(0);
            }
            "--drops-only" => config.drops_only = true,
            "--window" => {
                index += 1;
                config.window_ns = parse_seconds(value(&args, index, "--window")?, "--window")?;
            }
            "--step" => {
                index += 1;
                config.step_ns = parse_seconds(value(&args, index, "--step")?, "--step")?;
            }
            "--drop-ratio" => {
                index += 1;
                config.drop_ratio = parse_ratio(value(&args, index, "--drop-ratio")?)?;
            }
            "--baseline-windows" => {
                index += 1;
                config.baseline_windows = parse_positive_usize(
                    value(&args, index, "--baseline-windows")?,
                    "--baseline-windows",
                )?;
            }
            "--min-baseline-count" => {
                index += 1;
                config.min_baseline_count = value(&args, index, "--min-baseline-count")?
                    .parse::<usize>()
                    .map_err(|_| {
                        "--min-baseline-count must be a non-negative integer".to_owned()
                    })?;
            }
            "--" => {
                index += 1;
                while index < args.len() {
                    set_input(&mut config, &args[index])?;
                    index += 1;
                }
                break;
            }
            _ if argument.starts_with("--window=") => {
                config.window_ns = parse_seconds(&argument[9..], "--window")?;
            }
            _ if argument.starts_with("--step=") => {
                config.step_ns = parse_seconds(&argument[7..], "--step")?;
            }
            _ if argument.starts_with("--drop-ratio=") => {
                config.drop_ratio = parse_ratio(&argument[13..])?;
            }
            _ if argument.starts_with("--baseline-windows=") => {
                config.baseline_windows =
                    parse_positive_usize(&argument[19..], "--baseline-windows")?;
            }
            _ if argument.starts_with("--min-baseline-count=") => {
                config.min_baseline_count = argument[21..].parse::<usize>().map_err(|_| {
                    "--min-baseline-count must be a non-negative integer".to_owned()
                })?;
            }
            _ if argument.starts_with('-') && argument != "-" => {
                return Err(format!("unknown option: {argument}\n\n{}", usage()));
            }
            _ => set_input(&mut config, argument)?,
        }
        index += 1;
    }

    if config.step_ns > config.window_ns {
        return Err(
            "--step must not exceed --window; otherwise windows are not sliding".to_owned(),
        );
    }
    Ok(config)
}

fn value<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("{option} requires a value"))
}

fn set_input(config: &mut Config, value: &str) -> Result<(), String> {
    if config.input.is_some() {
        return Err("only one input file may be specified".to_owned());
    }
    if value != "-" {
        config.input = Some(PathBuf::from(value));
    }
    Ok(())
}

fn parse_seconds(value: &str, option: &str) -> Result<i128, String> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| format!("{option} must be a positive number of seconds"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("{option} must be a positive number of seconds"));
    }
    let nanos = seconds * NANOS_PER_SECOND as f64;
    if nanos > i128::MAX as f64 {
        return Err(format!("{option} is too large"));
    }
    Ok(nanos.round() as i128)
}

fn parse_ratio(value: &str) -> Result<f64, String> {
    let ratio = value
        .parse::<f64>()
        .map_err(|_| "--drop-ratio must be a number greater than 0 and less than 1".to_owned())?;
    if !ratio.is_finite() || !(0.0..1.0).contains(&ratio) {
        return Err("--drop-ratio must be greater than 0 and less than 1".to_owned());
    }
    Ok(ratio)
}

fn parse_positive_usize(value: &str, option: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{option} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{option} must be a positive integer"));
    }
    Ok(parsed)
}

fn parse_log<R: BufRead>(mut reader: R) -> Result<ParsedLog, String> {
    let mut events = Vec::new();
    let mut line = String::new();
    let mut lines = 0;
    let mut timestamped_lines = 0;
    let mut malformed_completions = 0;
    let mut last_timestamp_ns = None;

    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("cannot read log: {error}"))?;
        if bytes == 0 {
            break;
        }
        lines += 1;

        let clean = strip_ansi(&line);
        let timestamp = parse_line_timestamp(&clean);
        if let Some(timestamp_ns) = timestamp {
            timestamped_lines += 1;
            last_timestamp_ns =
                Some(last_timestamp_ns.map_or(timestamp_ns, |old: i128| old.max(timestamp_ns)));
        }

        if clean.contains(FINISH_MARKER) {
            if let Some(timestamp_ns) = timestamp {
                if let Some(event) = parse_completion(&clean, timestamp_ns) {
                    events.push(event);
                } else {
                    malformed_completions += 1;
                }
            } else {
                malformed_completions += 1;
            }
        }
    }

    events.sort_by_key(|event| event.timestamp_ns);
    let first_event_ns = events
        .first()
        .map(|event| event.timestamp_ns)
        .ok_or_else(|| {
            format!(
                "no completed syscall records found in {lines} lines; expected '{FINISH_MARKER}'"
            )
        })?;
    let last_event_ns = events.last().expect("events is not empty").timestamp_ns;

    Ok(ParsedLog {
        events,
        first_event_ns,
        last_timestamp_ns: last_timestamp_ns
            .unwrap_or(last_event_ns)
            .max(last_event_ns),
        lines,
        timestamped_lines,
        malformed_completions,
    })
}

fn parse_completion(line: &str, timestamp_ns: i128) -> Option<Event> {
    let dtid_start = line.find(DTID_MARKER)? + DTID_MARKER.len();
    let dtid_end = line[dtid_start..].find(']')? + dtid_start;
    let dtid = line[dtid_start..dtid_end].trim().parse::<u64>().ok()?;

    let syscall_start = line.find(FINISH_MARKER)? + FINISH_MARKER.len();
    let syscall_digits = line[syscall_start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let syscall_number = syscall_digits.parse::<u64>().ok()?;

    Some(Event {
        timestamp_ns,
        dtid,
        syscall_number,
    })
}

fn strip_ansi(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (b'@'..=b'~').contains(&byte) {
                    break;
                }
            }
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn parse_line_timestamp(line: &str) -> Option<i128> {
    line.split_whitespace().find_map(parse_rfc3339_nanos)
}

fn parse_rfc3339_nanos(value: &str) -> Option<i128> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || bytes.last() != Some(&b'Z')
    {
        return None;
    }

    let year = parse_decimal(&value[0..4])? as i64;
    let month = parse_decimal(&value[5..7])? as i64;
    let day = parse_decimal(&value[8..10])? as i64;
    let hour = parse_decimal(&value[11..13])? as i64;
    let minute = parse_decimal(&value[14..16])? as i64;
    let second = parse_decimal(&value[17..19])? as i64;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let mut fractional_nanos = 0_i128;
    if bytes.len() > 20 {
        if bytes.get(19) != Some(&b'.') {
            return None;
        }
        let fraction = &value[20..value.len() - 1];
        if fraction.is_empty()
            || fraction.len() > 9
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        fractional_nanos = parse_decimal(fraction)? as i128;
        for _ in fraction.len()..9 {
            fractional_nanos *= 10;
        }
    }

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(seconds as i128 * NANOS_PER_SECOND + fractional_nanos)
}

fn parse_decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok()
}

fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn analyze(parsed: &ParsedLog, config: &Config) -> Vec<Row> {
    let mut by_dtid: BTreeMap<u64, Vec<Event>> = BTreeMap::new();
    for event in &parsed.events {
        by_dtid.entry(event.dtid).or_default().push(event.clone());
    }

    let mut rows = analyze_series(None, &parsed.events, parsed, config);
    for (dtid, events) in by_dtid {
        rows.extend(analyze_series(Some(dtid), &events, parsed, config));
    }
    rows.sort_by(|left, right| {
        left.start_ns
            .cmp(&right.start_ns)
            .then_with(|| left.dtid.cmp(&right.dtid))
    });
    rows
}

fn analyze_series(
    dtid: Option<u64>,
    events: &[Event],
    parsed: &ParsedLog,
    config: &Config,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut history = VecDeque::new();
    let mut lower = 0;
    let mut upper = 0;
    let span_ns = parsed.last_timestamp_ns - parsed.first_event_ns;
    let last_start = if span_ns >= config.window_ns {
        parsed.last_timestamp_ns - config.window_ns
    } else {
        parsed.first_event_ns
    };
    let window_seconds = config.window_ns as f64 / NANOS_PER_SECOND as f64;
    let mut start_ns = parsed.first_event_ns;

    loop {
        let end_ns = start_ns + config.window_ns;
        while lower < events.len() && events[lower].timestamp_ns < start_ns {
            lower += 1;
        }
        upper = upper.max(lower);
        while upper < events.len() && events[upper].timestamp_ns < end_ns {
            upper += 1;
        }

        let count = upper - lower;
        let rate = count as f64 / window_seconds;
        let baseline = median(history.iter().copied());
        let drop = baseline.is_some_and(|baseline_rate| {
            baseline_rate * window_seconds >= config.min_baseline_count as f64
                && rate <= baseline_rate * config.drop_ratio
        });
        let first_syscall = (count > 0).then(|| events[lower].syscall_number);
        let last_syscall = (count > 0).then(|| events[upper - 1].syscall_number);

        if dtid.is_none() || count > 0 || drop {
            rows.push(Row {
                start_ns,
                end_ns,
                dtid,
                count,
                rate,
                baseline,
                drop,
                first_syscall,
                last_syscall,
            });
        }

        history.push_back(rate);
        if history.len() > config.baseline_windows {
            history.pop_front();
        }
        if start_ns >= last_start {
            break;
        }
        start_ns = (start_ns + config.step_ns).min(last_start);
    }
    rows
}

fn median<I>(values: I) -> Option<f64>
where
    I: IntoIterator<Item = f64>,
{
    let mut values: Vec<f64> = values.into_iter().collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        Some((values[middle - 1] + values[middle]) / 2.0)
    } else {
        Some(values[middle])
    }
}

fn render(source: &str, parsed: &ParsedLog, config: &Config, rows: &[Row]) -> String {
    let span_seconds = seconds(parsed.last_timestamp_ns - parsed.first_event_ns);
    let window_seconds = seconds(config.window_ns);
    let step_seconds = seconds(config.step_ns);
    let dtids = parsed
        .events
        .iter()
        .map(|event| event.dtid)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let mut output = String::new();

    output.push_str(&format!(
        "# source={source} lines={} timestamped={} completed_syscalls={} dtids={} span={span_seconds:.3}s\n",
        parsed.lines,
        parsed.timestamped_lines,
        parsed.events.len(),
        dtids
    ));
    output.push_str(&format!(
        "# window={window_seconds:.3}s step={step_seconds:.3}s baseline=median(previous {}) drop<= {:.0}% min_baseline_count={}\n",
        config.baseline_windows,
        config.drop_ratio * 100.0,
        config.min_baseline_count
    ));
    if parsed.malformed_completions > 0 {
        output.push_str(&format!(
            "# warning: {} completion lines could not be parsed\n",
            parsed.malformed_completions
        ));
    }
    output.push_str(&format!(
        "{:>10} {:>10} {:>8} {:>8} {:>11} {:>11} {:>9} {:>17}  {}\n",
        "start_s", "end_s", "dtid", "count", "rate/s", "baseline", "change", "syscalls", "status"
    ));

    for row in rows.iter().filter(|row| !config.drops_only || row.drop) {
        let dtid = row
            .dtid
            .map_or_else(|| "all".to_owned(), |value| value.to_string());
        let baseline = row
            .baseline
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.1}"));
        let change = row
            .baseline
            .and_then(|value| (value > 0.0).then_some((row.rate / value - 1.0) * 100.0));
        let change = change.map_or_else(|| "-".to_owned(), |value| format!("{value:+.0}%"));
        let syscalls = match (row.first_syscall, row.last_syscall) {
            (Some(first), Some(last)) if first == last => first.to_string(),
            (Some(first), Some(last)) => format!("{first}-{last}"),
            _ => "-".to_owned(),
        };
        let status = if row.drop { "!! DROP" } else { "" };

        output.push_str(&format!(
            "{:>10.3} {:>10.3} {:>8} {:>8} {:>11.1} {:>11} {:>9} {:>17}  {}\n",
            seconds(row.start_ns - parsed.first_event_ns),
            seconds(row.end_ns - parsed.first_event_ns),
            dtid,
            row.count,
            row.rate,
            baseline,
            change,
            syscalls,
            status
        ));
    }

    let drops: Vec<&Row> = rows.iter().filter(|row| row.drop).collect();
    output.push_str(&format!("# detected_drop_windows={}\n", drops.len()));
    let worst = drops
        .iter()
        .copied()
        .filter(|row| row.dtid.is_none())
        .min_by(|left, right| drop_fraction(left).total_cmp(&drop_fraction(right)))
        .or_else(|| {
            drops
                .iter()
                .copied()
                .min_by(|left, right| drop_fraction(left).total_cmp(&drop_fraction(right)))
        });
    if let Some(worst) = worst {
        let label = worst
            .dtid
            .map_or_else(|| "all".to_owned(), |dtid| format!("dtid {dtid}"));
        let baseline = worst.baseline.unwrap_or(0.0);
        output.push_str(&format!(
            "# worst_drop={} at {:.3}s: {:.1}/s versus {:.1}/s baseline ({:.0}% below), syscalls={}\n",
            label,
            seconds(worst.start_ns - parsed.first_event_ns),
            worst.rate,
            baseline,
            (1.0 - drop_fraction(worst)) * 100.0,
            match (worst.first_syscall, worst.last_syscall) {
                (Some(first), Some(last)) => format!("{first}-{last}"),
                _ => "-".to_owned(),
            }
        ));
    }
    output
}

fn drop_fraction(row: &Row) -> f64 {
    row.baseline
        .filter(|baseline| *baseline > 0.0)
        .map_or(1.0, |baseline| row.rate / baseline)
}

fn seconds(nanos: i128) -> f64 {
    nanos as f64 / NANOS_PER_SECOND as f64
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const PREFIX: &str =
        "2026-07-23T22:37:57.802837Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] ";

    #[test]
    fn parses_rfc3339_fraction_and_day_boundary() {
        let first = parse_rfc3339_nanos("2026-07-23T23:59:59.500000000Z").unwrap();
        let second = parse_rfc3339_nanos("2026-07-24T00:00:00.250000Z").unwrap();
        assert_eq!(second - first, 750_000_000);
    }

    #[test]
    fn strips_ansi_and_parses_completion_once() {
        let line = format!("\x1b[2m{PREFIX}finish syscall #850: brk(NULL) = Ok(0)\x1b[0m\n");
        let parsed = parse_log(Cursor::new(format!(
            "{PREFIX}inbound syscall: brk(NULL) = ?\n{line}"
        )))
        .unwrap();
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].dtid, 3);
        assert_eq!(parsed.events[0].syscall_number, 850);
    }

    #[test]
    fn detects_rate_drop_against_rolling_median() {
        let base = parse_rfc3339_nanos("2026-07-23T00:00:00Z").unwrap();
        let mut events = Vec::new();
        for second in 0..2 {
            for offset in 0..10 {
                events.push(Event {
                    timestamp_ns: base + second * NANOS_PER_SECOND + offset * 10_000_000,
                    dtid: 3,
                    syscall_number: (second * 10 + offset) as u64,
                });
            }
        }
        events.push(Event {
            timestamp_ns: base + 2 * NANOS_PER_SECOND,
            dtid: 3,
            syscall_number: 20,
        });
        let parsed = ParsedLog {
            events,
            first_event_ns: base,
            last_timestamp_ns: base + 3 * NANOS_PER_SECOND,
            lines: 21,
            timestamped_lines: 21,
            malformed_completions: 0,
        };
        let config = Config {
            step_ns: NANOS_PER_SECOND,
            baseline_windows: 2,
            ..Config::default()
        };
        let rows = analyze(&parsed, &config);
        let drop = rows
            .iter()
            .find(|row| row.dtid == Some(3) && row.start_ns == base + 2 * NANOS_PER_SECOND)
            .unwrap();
        assert!(drop.drop);
        assert_eq!(drop.count, 1);
        assert_eq!(drop.baseline, Some(10.0));

        let output = render("test.log", &parsed, &config, &rows);
        assert!(output.contains("# worst_drop=all"));
    }

    #[test]
    fn keeps_thread_rates_separate() {
        let base = parse_rfc3339_nanos("2026-07-23T00:00:00Z").unwrap();
        let parsed = ParsedLog {
            events: vec![
                Event {
                    timestamp_ns: base,
                    dtid: 3,
                    syscall_number: 1,
                },
                Event {
                    timestamp_ns: base + 1,
                    dtid: 5,
                    syscall_number: 1,
                },
            ],
            first_event_ns: base,
            last_timestamp_ns: base + NANOS_PER_SECOND,
            lines: 2,
            timestamped_lines: 2,
            malformed_completions: 0,
        };
        let rows = analyze(&parsed, &Config::default());
        assert!(rows.iter().any(|row| row.dtid == Some(3) && row.count == 1));
        assert!(rows.iter().any(|row| row.dtid == Some(5) && row.count == 1));
        assert!(rows.iter().any(|row| row.dtid.is_none() && row.count == 2));
    }
}
