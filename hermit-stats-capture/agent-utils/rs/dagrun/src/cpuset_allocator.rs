//! `cpuset-alloc`: reserve disjoint CPUs and run a command in a hard AllowedCPUs scope.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::reservation;
use crate::VERSION;

const PROG: &str = "cpuset-alloc";

fn usage() -> &'static str {
    "usage: cpuset-alloc <command> [options]\n\n\
Stateful cpuset allocator: reserve DISJOINT cores and HARD-pin a process tree to them.\n\n\
commands:\n\
  run       reserve K cores and run CMD's whole tree pinned to them\n\
  status    print live held cores\n\
  reclaim   sweep and print dead-holder reservations\n\
  selftest  mutation-test whether systemd AllowedCPUs is a hard bound\n\n\
options:\n\
  -h, --help  show this help\n\
  --version   show the version\n"
}

fn command_help(command: &str) -> &'static str {
    match command {
        "run" => {
            "usage: cpuset-alloc run --cores K [--tag STR] [--sample-s SECS] [--max-irq-rate RATE] -- CMD [ARGS...]\n"
        }
        "status" => "usage: cpuset-alloc status [--ledger FILE]\n",
        "reclaim" => "usage: cpuset-alloc reclaim [--ledger FILE]\n",
        "selftest" => "usage: cpuset-alloc selftest [--cores K] [--sample-s SECS] [--max-irq-rate RATE]\n",
        _ => usage(),
    }
}

fn parse_value(
    args: &[String],
    index: &mut usize,
    inline: Option<&str>,
    flag: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value.to_string());
    }
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn cpulist(cores: &[usize]) -> String {
    let mut sorted = cores.to_vec();
    sorted.sort_unstable();
    sorted
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn scope_command(cores: &[usize], command: &[String], tag: &str) -> Command {
    let mut out = Command::new("systemd-run");
    out.process_group(0);
    out.args([
        "--user",
        "--scope",
        "--collect",
        "--quiet",
        "-p",
        &format!("AllowedCPUs={}", cpulist(cores)),
    ]);
    if !tag.is_empty() {
        let safe: String = tag
            .chars()
            .map(|ch| if ch.is_alphanumeric() { ch } else { '-' })
            .take(48)
            .collect();
        out.args([
            "--unit",
            &format!("cpuset-alloc-{safe}-{}", std::process::id()),
        ]);
    }
    out.arg("--").args(command);
    out
}

fn status_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn systemd_run_available() -> bool {
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect", "true"])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    status_with_timeout(&mut command, Duration::from_secs(15))
        .is_ok_and(|status| status.is_some_and(|status| status.success()))
}

fn allowed_from_status(status: &[u8]) -> Option<String> {
    crate::procstat::status_field(status, b"Cpus_allowed_list")
        .ok()
        .flatten()
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(str::to_owned)
}

fn read_allowed(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| allowed_from_status(&status))
        .unwrap_or_default()
}

fn set_pid_affinity(pid: u32, cores: &[usize]) -> std::io::Result<()> {
    let limit = 8 * std::mem::size_of::<libc::cpu_set_t>();
    if cores.iter().any(|core| *core >= limit) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CPU id exceeds cpu_set_t",
        ));
    }
    // SAFETY: the set is initialized, every bit is range-checked above, and libc reads it only
    // for the duration of the call.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut set) };
    for core in cores {
        unsafe { libc::CPU_SET(*core, &mut set) };
    }
    let rc = unsafe {
        libc::sched_setaffinity(
            pid as libc::pid_t,
            std::mem::size_of::<libc::cpu_set_t>(),
            &set,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn parse_cpulist(spec: &str) -> Option<Vec<usize>> {
    let mut cores = Vec::new();
    for part in spec.split(',').filter(|part| !part.is_empty()) {
        if let Some((lo, hi)) = part.split_once('-') {
            let (lo, hi) = (lo.parse::<usize>().ok()?, hi.parse::<usize>().ok()?);
            if hi < lo {
                return None;
            }
            cores.extend(lo..=hi);
        } else {
            cores.push(part.parse().ok()?);
        }
    }
    cores.sort_unstable();
    cores.dedup();
    (!cores.is_empty()).then_some(cores)
}

fn current_allowed() -> Vec<usize> {
    parse_cpulist(&read_allowed(std::process::id())).unwrap_or_default()
}

fn hidden_probe(excluded: usize, assigned: &[usize]) -> i32 {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{PROG}: probe: {error}");
            return 3;
        }
    };
    let mut child = match Command::new(&executable).arg("__hold").spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("{PROG}: probe: {error}");
            return 3;
        }
    };
    std::thread::sleep(Duration::from_millis(200));
    let before = read_allowed(child.id());
    let mutation = set_pid_affinity(child.id(), &[excluded]);
    let mutation_blocked = mutation.is_err();
    let mutation_error = mutation
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    let after = read_allowed(child.id());
    let _ = child.kill();
    let _ = child.wait();

    let mut usable = Vec::new();
    for core in assigned {
        if set_pid_affinity(0, &[*core]).is_ok() && current_allowed() == [*core] {
            usable.push(*core);
        }
    }
    let mut expected = assigned.to_vec();
    expected.sort_unstable();
    let restore_exact = set_pid_affinity(0, assigned).is_ok() && {
        let mut restored = current_allowed();
        restored.sort_unstable();
        restored == expected
    };
    println!(
        "{}",
        serde_json::json!({
            "child_allowed_before": before,
            "child_allowed_after": after,
            "mutation_attempted": true,
            "mutation_blocked": mutation_blocked,
            "mutation_error": mutation_error,
            "positive_cores_usable": usable,
            "restore_exact": restore_exact,
        })
    );
    0
}

fn hold() -> i32 {
    std::thread::sleep(Duration::from_secs(5));
    0
}

fn evaluate_probe(cores: &[usize], excluded: usize, inner: &Value) -> Value {
    let attempted = inner["mutation_attempted"].as_bool().unwrap_or(false);
    let blocked = inner["mutation_blocked"].as_bool().unwrap_or(false);
    let unchanged = inner["child_allowed_before"].as_str().is_some()
        && inner["child_allowed_before"] == inner["child_allowed_after"];
    let mut usable: Vec<usize> = inner["positive_cores_usable"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_u64().map(|core| core as usize))
        .collect();
    usable.sort_unstable();
    let mut wanted = cores.to_vec();
    wanted.sort_unstable();
    let positive_ok = usable == wanted && inner["restore_exact"].as_bool().unwrap_or(false);
    let negative_ok = attempted && blocked && unchanged;
    let verdict = if negative_ok && positive_ok {
        "HARD"
    } else {
        "SOFT_OR_INERT"
    };
    serde_json::json!({
        "verdict": verdict,
        "cores": cores,
        "count": cores.len(),
        "excluded_core": excluded,
        "mutation_attempted": attempted,
        "mutation_blocked": blocked,
        "negative_escape_masked": negative_ok,
        "positive_cores_usable": usable,
        "positive_cores_used": usable,
        "positive_stayed_in_set": positive_ok,
        "inner": inner,
    })
}

fn probe_hard_pin(cores: &[usize]) -> Value {
    let assigned: std::collections::HashSet<usize> = cores.iter().copied().collect();
    let Some(excluded) = current_allowed()
        .into_iter()
        .find(|core| !assigned.contains(core))
    else {
        return serde_json::json!({"verdict": "UNTESTABLE", "reason": "no core outside the reserved set"});
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return serde_json::json!({"verdict": "ERROR", "reason": error.to_string()});
        }
    };
    let command = vec![
        executable.display().to_string(),
        "__probe".to_string(),
        excluded.to_string(),
        cpulist(cores),
    ];
    let mut scope = scope_command(cores, &command, "");
    let output = match output_with_timeout(&mut scope, Duration::from_secs(60)) {
        Ok(Some(output)) => output,
        Ok(None) => {
            return serde_json::json!({"verdict": "ERROR", "reason": "scope probe timed out"});
        }
        Err(error) => {
            return serde_json::json!({"verdict": "ERROR", "reason": error.to_string()});
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(inner) = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .next_back()
    else {
        return serde_json::json!({
            "verdict": "ERROR",
            "reason": "scope probe produced no result",
            "returncode": output.status.code(),
            "stderr": String::from_utf8_lossy(&output.stderr).trim(),
        });
    };
    evaluate_probe(cores, excluded, &inner)
}

fn cmd_status(args: &[String]) -> i32 {
    let ledger = match ledger_flag(args) {
        Ok(path) => path,
        Err(error) => return usage_error("status", &error),
    };
    match reservation::held_cores(ledger.as_deref()) {
        Ok(cores) => {
            let rendered = serde_json::to_string_pretty(&cores).unwrap_or_else(|_| "[]".into());
            let indented = rendered.replace('\n', "\n  ");
            println!(
                "{{\n  \"held_cores\": {},\n  \"held_count\": {}\n}}",
                indented,
                cores.len()
            );
            0
        }
        Err(error) => operational_error("status", &error),
    }
}

fn cmd_reclaim(args: &[String]) -> i32 {
    let ledger = match ledger_flag(args) {
        Ok(path) => path,
        Err(error) => return usage_error("reclaim", &error),
    };
    match reservation::reclaim_dead(ledger.as_deref()) {
        Ok(records) => {
            let rendered = serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".into());
            let indented = rendered.replace('\n', "\n  ");
            println!(
                "{{\n  \"reclaimed\": {},\n  \"reclaimed_count\": {}\n}}",
                indented,
                records.len()
            );
            0
        }
        Err(error) => operational_error("reclaim", &error),
    }
}

fn ledger_flag(args: &[String]) -> Result<Option<PathBuf>, String> {
    let mut ledger = None;
    let mut i = 0;
    while i < args.len() {
        let (key, inline) = match args[i].split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (args[i].as_str(), None),
        };
        if key != "--ledger" {
            return Err(format!("unrecognized argument: {key}"));
        }
        ledger = Some(PathBuf::from(parse_value(
            args, &mut i, inline, "--ledger",
        )?));
        i += 1;
    }
    Ok(ledger)
}

/// Parsed `run`/`selftest` options. A named struct rather than a tuple so adding a knob does
/// not silently renumber every call site's field access.
struct CoreOptions {
    cores: i64,
    tag: String,
    sample_s: f64,
    max_irq_rate: Option<f64>,
    command: Vec<String>,
}

fn parse_core_options(
    args: &[String],
    default_cores: Option<i64>,
    allow_tag: bool,
) -> Result<CoreOptions, String> {
    let mut cores = default_cores;
    let mut tag = String::new();
    let mut sample_s: f64 = 0.3;
    // No default: a caller that knows its interrupt budget states it. Measurement showed any
    // fixed threshold would misclassify a bursty signal, so the tool does not invent one.
    let mut max_irq_rate: Option<f64> = None;
    let mut command = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            command.extend_from_slice(&args[i + 1..]);
            break;
        }
        let (key, inline) = match args[i].split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (args[i].as_str(), None),
        };
        match key {
            "--cores" => {
                let raw = parse_value(args, &mut i, inline, key)?;
                cores = Some(raw.parse().map_err(|_| format!("invalid --cores: {raw}"))?);
            }
            "--tag" if allow_tag => tag = parse_value(args, &mut i, inline, key)?,
            "--sample-s" => {
                let raw = parse_value(args, &mut i, inline, key)?;
                sample_s = raw
                    .parse()
                    .map_err(|_| format!("invalid --sample-s: {raw}"))?;
            }
            "--max-irq-rate" => {
                let raw = parse_value(args, &mut i, inline, key)?;
                let value: f64 = raw
                    .parse()
                    .map_err(|_| format!("invalid --max-irq-rate: {raw}"))?;
                max_irq_rate = Some(value);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
        i += 1;
    }
    let cores = cores.ok_or_else(|| "--cores is required".to_string())?;
    if cores < 1 {
        return Err("--cores must be >= 1".to_string());
    }
    if !sample_s.is_finite() || sample_s < 0.0 {
        return Err("--sample-s must be finite and >= 0".to_string());
    }
    if let Some(value) = max_irq_rate {
        if !value.is_finite() || value < 0.0 {
            return Err("--max-irq-rate must be finite and >= 0".to_string());
        }
        if sample_s <= 0.0 {
            return Err("--sample-s must be > 0 when --max-irq-rate is set".to_string());
        }
    }
    Ok(CoreOptions {
        cores,
        tag,
        sample_s,
        max_irq_rate,
        command,
    })
}

fn wrapped_status(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

fn executable_available(command: &str) -> bool {
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        return executable(Path::new(command));
    }
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|directory| executable(&directory.join(command)))
        })
        .unwrap_or(false)
}

/// Run a command in a mutation-verified AllowedCPUs scope over already-reserved cores.
pub(crate) fn run_reserved_hard(
    cores: &[usize],
    command: &[String],
    tag: &str,
    program: &str,
) -> i32 {
    if command
        .first()
        .is_none_or(|executable| !executable_available(executable))
    {
        let missing = command.first().map(String::as_str).unwrap_or("<empty>");
        eprintln!("{program}: executable not found or not executable: {missing}");
        return 3;
    }
    if !systemd_run_available() {
        eprintln!(
            "{program}: `systemd-run --user --scope` is unavailable here, so a HARD cpuset pin \
             cannot be applied. Refusing to run un-pinned."
        );
        return 3;
    }
    let probe = probe_hard_pin(cores);
    if probe["verdict"] != "HARD" {
        eprintln!(
            "{program}: reserved cores could not be mutation-verified as a HARD tree-wide pin; \
             refusing to run: {probe}"
        );
        return 3;
    }
    eprintln!(
        "{program}: reserved {{\"cores\":{},\"count\":{}}}; running whole tree pinned via AllowedCPUs={}",
        serde_json::to_string(cores).unwrap_or_default(),
        cores.len(),
        cpulist(cores)
    );
    match scope_command(cores, command, tag).status() {
        Ok(status) => wrapped_status(status),
        Err(error) => {
            eprintln!("{program}: failed to launch scope ({error})");
            3
        }
    }
}

fn cmd_run(args: &[String]) -> i32 {
    let CoreOptions {
        cores,
        tag,
        sample_s,
        max_irq_rate,
        command,
    } = match parse_core_options(args, None, true) {
        Ok(value) => value,
        Err(error) => return usage_error("run", &error),
    };
    if command.is_empty() {
        return usage_error("run", "no command given (use `-- CMD ARGS...`)");
    }
    let mut held = match reservation::acquire(
        cores,
        &tag,
        sample_s,
        None,
        &std::collections::HashSet::new(),
        max_irq_rate,
    ) {
        Ok(value) => value,
        Err(error) => return operational_error("run", &error),
    };
    let code = run_reserved_hard(&held.cores, &command, &tag, &format!("{PROG}: run"));
    let _ = held.release();
    code
}

fn cmd_selftest(args: &[String]) -> i32 {
    let CoreOptions {
        cores,
        sample_s,
        max_irq_rate,
        command,
        ..
    } = match parse_core_options(args, Some(2), false) {
        Ok(value) => value,
        Err(error) => return usage_error("selftest", &error),
    };
    if !command.is_empty() {
        return usage_error("selftest", "unexpected command arguments");
    }
    if !systemd_run_available() {
        println!(
            "{}",
            serde_json::json!({"verdict":"UNTESTABLE","reason":"systemd-run --user --scope unavailable"})
        );
        return 3;
    }
    let held = match reservation::acquire(
        cores,
        "selftest",
        sample_s,
        None,
        &std::collections::HashSet::new(),
        max_irq_rate,
    ) {
        Ok(value) => value,
        Err(error) => return operational_error("selftest", &error),
    };
    let result = probe_hard_pin(&held.cores);
    println!(
        "{}",
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
    );
    match result["verdict"].as_str() {
        Some("HARD") => 0,
        Some("SOFT_OR_INERT") => 1,
        _ => 3,
    }
}

fn usage_error(command: &str, error: &str) -> i32 {
    eprintln!("{PROG}: {command}: {error}");
    2
}

fn operational_error(command: &str, error: &str) -> i32 {
    eprintln!("{PROG}: {command}: {error}");
    3
}

fn requests_help(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "-h" || arg == "--help")
}

/// Run `cpuset-alloc` over arguments excluding the program name.
pub fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        print!("{}", usage());
        return 0;
    }
    match args[0].as_str() {
        "-h" | "--help" => {
            print!("{}", usage());
            0
        }
        "--version" => {
            println!("{PROG} {VERSION}");
            0
        }
        "__hold" => hold(),
        "__probe" if args.len() == 3 => match (args[1].parse(), parse_cpulist(&args[2])) {
            (Ok(excluded), Some(assigned)) => hidden_probe(excluded, &assigned),
            _ => 2,
        },
        command @ ("run" | "status" | "reclaim" | "selftest") => {
            if requests_help(&args[1..]) {
                print!("{}", command_help(command));
                return 0;
            }
            match command {
                "run" => cmd_run(&args[1..]),
                "status" => cmd_status(&args[1..]),
                "reclaim" => cmd_reclaim(&args[1..]),
                "selftest" => cmd_selftest(&args[1..]),
                _ => unreachable!(),
            }
        }
        command => usage_error("", &format!("unknown command: {command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpulist_is_sorted() {
        assert_eq!(cpulist(&[7, 2, 4]), "2,4,7");
    }

    #[test]
    fn allowed_cpu_field_cannot_be_forged_by_opaque_name_bytes() {
        let valid = b"Name:\tbad\xff\rCpus_allowed_list:\t99\x0bmore\nState:\tS\nCpus_allowed_list:\t0-2,7\n";
        assert_eq!(allowed_from_status(valid).as_deref(), Some("0-2,7"));

        let forged =
            b"Name:\tbad\xff\nCpus_allowed_list:\t99\nState:\tS\nCpus_allowed_list:\t0-2,7\n";
        assert_eq!(allowed_from_status(forged), None);
    }

    #[test]
    fn parse_options_accepts_inline_values() {
        let args = vec![
            "--cores=2".into(),
            "--tag=x".into(),
            "--sample-s=0.01".into(),
            "--max-irq-rate=12.5".into(),
            "--".into(),
            "true".into(),
        ];
        let parsed = parse_core_options(&args, None, true).unwrap();
        assert_eq!(parsed.cores, 2);
        assert_eq!(parsed.tag, "x");
        assert_eq!(parsed.max_irq_rate, Some(12.5));
        assert_eq!(parsed.command, vec!["true"]);
    }

    #[test]
    fn max_irq_rate_is_absent_by_default_and_validated_when_given() {
        // No default: measurement showed a fixed threshold misclassifies a bursty signal.
        let parsed = parse_core_options(&["--cores=1".into()], None, true).unwrap();
        assert_eq!(parsed.max_irq_rate, None);
        assert!(parse_core_options(
            &["--cores=1".into(), "--max-irq-rate=-1".into()],
            None,
            true
        )
        .is_err());
        assert!(parse_core_options(
            &["--cores=1".into(), "--max-irq-rate=NaN".into()],
            None,
            true
        )
        .is_err());
        assert!(parse_core_options(
            &[
                "--cores=1".into(),
                "--sample-s=0".into(),
                "--max-irq-rate=1".into()
            ],
            None,
            true
        )
        .is_err());
    }

    #[test]
    fn parser_rejects_invalid_counts_samples_and_selftest_tags() {
        assert!(parse_core_options(&["--cores=0".into()], None, true).is_err());
        assert!(
            parse_core_options(&["--cores=1".into(), "--sample-s=NaN".into()], None, true).is_err()
        );
        assert!(parse_core_options(&["--tag=x".into()], Some(1), false).is_err());
    }

    #[test]
    fn wrapped_command_help_is_not_allocator_help() {
        let args = vec![
            "--cores=1".into(),
            "--".into(),
            "printf".into(),
            "--help".into(),
        ];
        assert!(!requests_help(&args));
    }

    #[test]
    fn probe_requires_blocked_mutation_and_every_assigned_core() {
        let incomplete = serde_json::json!({
            "child_allowed_before": "2-3",
            "child_allowed_after": "2-3",
            "mutation_attempted": true,
            "mutation_blocked": true,
            "positive_cores_usable": [2],
            "restore_exact": true,
        });
        assert_eq!(
            evaluate_probe(&[2, 3], 4, &incomplete)["verdict"],
            "SOFT_OR_INERT"
        );
        let not_attempted = serde_json::json!({
            "child_allowed_before": "2-3",
            "child_allowed_after": "2-3",
            "mutation_attempted": false,
            "mutation_blocked": false,
            "positive_cores_usable": [2, 3],
            "restore_exact": true,
        });
        assert_eq!(
            evaluate_probe(&[2, 3], 4, &not_attempted)["verdict"],
            "SOFT_OR_INERT"
        );
        let complete = serde_json::json!({
            "child_allowed_before": "2-3",
            "child_allowed_after": "2-3",
            "mutation_attempted": true,
            "mutation_blocked": true,
            "positive_cores_usable": [3, 2],
            "restore_exact": true,
        });
        assert_eq!(evaluate_probe(&[2, 3], 4, &complete)["verdict"], "HARD");
    }

    #[test]
    fn signal_status_uses_shell_convention() {
        let status = Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(wrapped_status(status), 143);
    }

    #[test]
    fn executable_preflight_rejects_missing_paths() {
        assert!(executable_available("true"));
        assert!(!executable_available("/definitely/missing/cpuset-command"));
    }
}
