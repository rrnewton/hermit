use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use dagrun::ManualCpuCgroup;
use dagrun::ManualCpuCgroupStatus;
use dagrun::SharedCpuCgroupParent;
use hermit_manifest_plan::nextest_cpu::AttemptCompletion;
use hermit_manifest_plan::nextest_cpu::AttemptIdentity;
use hermit_manifest_plan::nextest_cpu::AttemptOutcome;
use hermit_manifest_plan::nextest_cpu::AttemptRecord;
use hermit_manifest_plan::nextest_cpu::BINARY_MAP_SCHEMA;
use hermit_manifest_plan::nextest_cpu::BinaryMap;
use hermit_manifest_plan::nextest_cpu::BinaryMapEntry;
use hermit_manifest_plan::nextest_cpu::CPU_BINARY_MAP_ENV;
use hermit_manifest_plan::nextest_cpu::CPU_RECORD_DIR_ENV;
use hermit_manifest_plan::nextest_cpu::CPU_REPORT_PATH_ENV;
use hermit_manifest_plan::nextest_cpu::read_attempt_records;
use hermit_manifest_plan::nextest_cpu::read_binary_map;
use hermit_manifest_plan::nextest_cpu::write_attempt_atomic;
use hermit_manifest_plan::nextest_cpu::write_binary_map_atomic;
use hermit_manifest_plan::timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS;
use hermit_manifest_plan::timeouts::DEFAULT_TEST_WALL_TIMEOUT_SECONDS;
use hermit_manifest_plan::timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV;
use hermit_manifest_plan::timeouts::TEST_WALL_TIMEOUT_MULTIPLIER_ENV;
use hermit_manifest_plan::timeouts::resolve_test_timeouts;
use hermit_manifest_plan::timeouts::timeout_multipliers_from_env;

const PRIVATE_ATTEMPT_ENV: &str = "__NEXTEST_ATTEMPT";
const PUBLIC_ATTEMPT_ENV: &str = "NEXTEST_ATTEMPT";
const RUN_ID_ENV: &str = "NEXTEST_RUN_ID";
const PACKAGE_ENV: &str = "CARGO_PKG_NAME";
const CONTROL_ARM_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL";
const CONTROL_CWD_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_CWD";
const CONTROL_PID_FILE_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_PID_FILE";
const CONTROL_SENTINEL_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_SENTINEL";
const CONTROL_CPU_LIMIT_USEC_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_LIMIT_USEC";
const CONTROL_WALL_LIMIT_MS_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_WALL_MS";
const CONTROL_POLL_MS_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_POLL_MS";
const CONTROL_ACCOUNTING_ERROR_ENV: &str = "HERMIT_NEXTEST_CPU_CONTROL_ACCOUNTING_ERROR";
const INFRASTRUCTURE_EXIT: u8 = 70;
const TIMEOUT_EXIT: u8 = 124;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_POLL: Duration = Duration::from_millis(10);

static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);
static INTERNAL_TERMINATION: AtomicBool = AtomicBool::new(false);

extern "C" fn remember_signal(signal: libc::c_int) {
    if !INTERNAL_TERMINATION.load(Ordering::SeqCst) {
        let _ = RECEIVED_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name).map_err(|error| format!("{name} must be present and valid UTF-8: {error}"))
}

fn identity_from_command(program: &OsStr, args: &[OsString]) -> Result<AttemptIdentity, String> {
    let mut command = Vec::with_capacity(args.len() + 1);
    command.push(program);
    command.extend(args.iter().map(OsString::as_os_str));
    let exact = command
        .windows(4)
        .find(|window| window[1] == OsStr::new("--exact") && window[3] == OsStr::new("--nocapture"))
        .ok_or_else(|| {
            "nextest wrapper command lacks the expected TEST_BINARY --exact TEST --nocapture sequence"
                .to_string()
        })?;
    let test = exact[2]
        .to_str()
        .ok_or_else(|| "nextest test name is not valid UTF-8".to_string())?;
    let map = read_binary_map(Path::new(&required_env(CPU_BINARY_MAP_ENV)?))?;
    let (package, binary) = map.identity_for_executable(Path::new(exact[0]))?;
    let command_package = required_env(PACKAGE_ENV)?;
    if command_package != package {
        return Err(format!(
            "nextest command package {command_package:?} disagrees with typed inventory package {package:?}"
        ));
    }
    let private = env::var(PRIVATE_ATTEMPT_ENV).ok();
    let public = env::var(PUBLIC_ATTEMPT_ENV).ok();
    if private.is_some() && public.is_some() && private != public {
        return Err(format!(
            "{PRIVATE_ATTEMPT_ENV} and {PUBLIC_ATTEMPT_ENV} disagree"
        ));
    }
    let raw_attempt = public.or(private).ok_or_else(|| {
        format!("{PUBLIC_ATTEMPT_ENV} or {PRIVATE_ATTEMPT_ENV} must be present and valid UTF-8")
    })?;
    let attempt = raw_attempt
        .parse::<u64>()
        .map_err(|error| format!("nextest attempt is not a positive integer: {error}"))?;
    let identity = AttemptIdentity {
        package: package.to_string(),
        binary: binary.to_string(),
        test: test.to_string(),
        attempt,
    };
    identity.validate()?;
    Ok(identity)
}

#[derive(Clone, Copy, Debug)]
struct AttemptLimits {
    cpu_usec: u64,
    wall_ms: u64,
    poll: Duration,
}

fn parse_positive_control(name: &str) -> Result<Option<u64>, String> {
    match env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse::<u64>()
                .map_err(|error| format!("{name} is not a positive integer: {error}"))?;
            if parsed == 0 {
                return Err(format!("{name} must be positive"));
            }
            Ok(Some(parsed))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} must be valid UTF-8")),
    }
}

fn attempt_limits() -> Result<AttemptLimits, String> {
    let resolved = resolve_test_timeouts(
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
        DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
        timeout_multipliers_from_env()?,
    )?;
    let mut cpu_usec = resolved
        .cpu_seconds
        .checked_mul(1_000_000)
        .ok_or_else(|| "CPU timeout overflows microseconds".to_string())?;
    let mut wall_ms = resolved
        .wall_seconds
        .checked_mul(1_000)
        .ok_or_else(|| "wall timeout overflows milliseconds".to_string())?;
    let mut poll = DEFAULT_POLL;
    if env::var_os(CONTROL_ARM_ENV).is_some() {
        if let Some(value) = parse_positive_control(CONTROL_CPU_LIMIT_USEC_ENV)? {
            cpu_usec = value;
        }
        if let Some(value) = parse_positive_control(CONTROL_WALL_LIMIT_MS_ENV)? {
            wall_ms = value;
        }
        if let Some(value) = parse_positive_control(CONTROL_POLL_MS_ENV)? {
            poll = Duration::from_millis(value);
        }
    }
    Ok(AttemptLimits {
        cpu_usec,
        wall_ms,
        poll,
    })
}

fn live_cpu_usage_usec(cgroup: &ManualCpuCgroup) -> Result<u64, String> {
    if env::var_os(CONTROL_ARM_ENV).is_some() && env::var_os(CONTROL_ACCOUNTING_ERROR_ENV).is_some()
    {
        return Err("injected malformed CPU accounting".into());
    }
    cgroup
        .cpu_usage_usec()
        .map_err(|error| format!("cannot read per-attempt CPU accounting: {error}"))
}

fn elapsed_ms(started: Instant) -> Result<u64, String> {
    u64::try_from(started.elapsed().as_millis())
        .map_err(|error| format!("attempt wall duration does not fit u64 milliseconds: {error}"))
}

fn install_signal_handlers() -> Result<(), String> {
    RECEIVED_SIGNAL.store(0, Ordering::SeqCst);
    INTERNAL_TERMINATION.store(false, Ordering::SeqCst);
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = remember_signal as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        action.sa_flags = 0;
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(format!(
                "cannot install signal handler for {signal}: {}",
                io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

fn completion_from_status(status: ExitStatus) -> Result<AttemptCompletion, String> {
    match (status.code(), status.signal()) {
        (Some(code), None) => Ok(AttemptCompletion::Exit { code }),
        (None, Some(signal)) => Ok(AttemptCompletion::Signal { signal }),
        _ => Err(format!("child returned unsupported exit status {status:?}")),
    }
}

fn propagate_signal(signal: i32) -> ! {
    let pid = std::process::id() as i32;
    unsafe {
        // The wrapper is the process-group leader established by nextest. Send
        // the signal to the complete test group before restoring the default
        // disposition for this process.
        libc::kill(-pid, signal);
        let mut action = std::mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, signal);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(signal);
        libc::_exit(128 + signal);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopCause {
    CpuTimeout,
    WallTimeout,
    Cancelled(i32),
}

fn signal_process_group(signal: i32) -> Result<(), String> {
    INTERNAL_TERMINATION.store(true, Ordering::SeqCst);
    let pid = std::process::id() as i32;
    if unsafe { libc::kill(-pid, signal) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "cannot signal test process group {pid} with {signal}: {}",
            io::Error::last_os_error()
        ))
    }
}

fn reap_and_empty(
    child: &mut Child,
    cgroup: &ManualCpuCgroup,
    status: &mut Option<ExitStatus>,
    graceful: bool,
) -> Result<(), String> {
    if graceful
        && cgroup.status().map_err(|error| error.to_string())? == ManualCpuCgroupStatus::Populated
    {
        signal_process_group(libc::SIGTERM)?;
        let deadline = Instant::now() + TERMINATION_GRACE;
        while Instant::now() < deadline {
            if status.is_none() {
                *status = child
                    .try_wait()
                    .map_err(|error| format!("cannot poll test command: {error}"))?;
            }
            if cgroup.status().map_err(|error| error.to_string())? == ManualCpuCgroupStatus::Empty {
                break;
            }
            thread::sleep(DEFAULT_POLL);
        }
    }
    if cgroup.status().map_err(|error| error.to_string())? == ManualCpuCgroupStatus::Populated {
        cgroup
            .kill()
            .map_err(|error| format!("cannot hard-kill per-attempt cgroup: {error}"))?;
    }
    if status.is_none() {
        *status = Some(
            child
                .wait()
                .map_err(|error| format!("cannot reap test command: {error}"))?,
        );
    }
    let deadline = Instant::now() + TERMINATION_GRACE;
    loop {
        if cgroup.status().map_err(|error| error.to_string())? == ManualCpuCgroupStatus::Empty {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("per-attempt cgroup remained populated after hard kill".into());
        }
        thread::sleep(DEFAULT_POLL);
    }
}

fn preserve_teardown_error(primary: String, cgroup: &ManualCpuCgroup) -> String {
    match cgroup.abort_and_cleanup() {
        Ok(()) => primary,
        Err(error) => format!("{primary}; per-attempt cgroup teardown also failed: {error}"),
    }
}

fn abort_and_reap(
    child: &mut Child,
    cgroup: &ManualCpuCgroup,
    status: &mut Option<ExitStatus>,
    primary: String,
) -> String {
    let mut detail = preserve_teardown_error(primary, cgroup);
    if status.is_none() {
        match child.try_wait() {
            Ok(Some(observed)) => *status = Some(observed),
            Ok(None) => {
                if let Err(error) = child.kill() {
                    detail.push_str(&format!("; cannot kill direct test child: {error}"));
                }
                match child.wait() {
                    Ok(observed) => *status = Some(observed),
                    Err(error) => {
                        detail.push_str(&format!("; cannot reap direct test child: {error}"));
                    }
                }
            }
            Err(error) => {
                detail.push_str(&format!("; cannot poll direct test child: {error}"));
                if let Err(kill) = child.kill() {
                    detail.push_str(&format!("; cannot kill direct test child: {kill}"));
                }
                if let Err(wait) = child.wait() {
                    detail.push_str(&format!("; cannot reap direct test child: {wait}"));
                }
            }
        }
    }
    detail
}

struct InfrastructureFailure {
    completion: Option<AttemptCompletion>,
    cpu_usage_usec: Option<u64>,
    detail: String,
}

fn write_infrastructure_record(
    record_dir: &Path,
    run_id: String,
    identity: AttemptIdentity,
    limits: AttemptLimits,
    started: Instant,
    failure: InfrastructureFailure,
) -> Result<ExitStatus, String> {
    let record = AttemptRecord::new(
        run_id,
        identity,
        hermit_manifest_plan::nextest_cpu::AttemptMeasurement {
            cpu_usage_usec: failure.cpu_usage_usec,
            cpu_limit_usec: limits.cpu_usec,
            wall_time_ms: elapsed_ms(started)?,
            wall_limit_ms: limits.wall_ms,
        },
        failure.completion,
        AttemptOutcome::InfrastructureError {
            detail: failure.detail.clone(),
        },
    );
    write_attempt_atomic(record_dir, &record).map_err(|write| {
        format!(
            "{}; cannot publish infrastructure record: {write}",
            failure.detail
        )
    })?;
    Err(failure.detail)
}

fn run_wrapper(args: Vec<OsString>) -> Result<ExitStatus, String> {
    let (program, child_args) = args
        .split_first()
        .ok_or_else(|| "nextest CPU wrapper requires a test command".to_string())?;
    let pid = std::process::id();
    let pgid = unsafe { libc::getpgrp() };
    if pgid != pid as i32 {
        return Err(format!(
            "nextest CPU wrapper PID {pid} is in process group {pgid}; refusing to attribute another process group's CPU"
        ));
    }
    let record_dir = PathBuf::from(required_env(CPU_RECORD_DIR_ENV)?);
    let run_id = required_env(RUN_ID_ENV)?;
    let identity = identity_from_command(program, child_args)?;
    let limits = attempt_limits()?;
    let started = Instant::now();
    install_signal_handlers()?;
    let parent = match SharedCpuCgroupParent::current() {
        Ok(parent) => parent,
        Err(error) => {
            return write_infrastructure_record(
                &record_dir,
                run_id,
                identity,
                limits,
                started,
                InfrastructureFailure {
                    completion: None,
                    cpu_usage_usec: None,
                    detail: format!("cannot resolve shared per-attempt cgroup parent: {error}"),
                },
            );
        }
    };
    let cgroup = match parent.create_child(&identity.key()) {
        Ok(cgroup) => cgroup,
        Err(error) => {
            return write_infrastructure_record(
                &record_dir,
                run_id,
                identity,
                limits,
                started,
                InfrastructureFailure {
                    completion: None,
                    cpu_usage_usec: None,
                    detail: format!("cannot create per-attempt CPU cgroup: {error}"),
                },
            );
        }
    };
    let mut command = Command::new(program);
    command.args(child_args);
    command.env_remove(CPU_BINARY_MAP_ENV);
    command.env_remove(CPU_RECORD_DIR_ENV);
    command.env_remove(CPU_REPORT_PATH_ENV);
    command.env_remove(TEST_CPU_TIMEOUT_MULTIPLIER_ENV);
    command.env_remove(TEST_WALL_TIMEOUT_MULTIPLIER_ENV);
    if let Err(error) = cgroup.attach_command(&mut command) {
        let detail = preserve_teardown_error(
            format!("cannot attach test command to per-attempt cgroup: {error}"),
            &cgroup,
        );
        return write_infrastructure_record(
            &record_dir,
            run_id,
            identity,
            limits,
            started,
            InfrastructureFailure {
                completion: None,
                cpu_usage_usec: None,
                detail,
            },
        );
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let detail = preserve_teardown_error(
                format!("cannot execute nextest test command: {error}"),
                &cgroup,
            );
            return write_infrastructure_record(
                &record_dir,
                run_id,
                identity,
                limits,
                started,
                InfrastructureFailure {
                    completion: None,
                    cpu_usage_usec: None,
                    detail,
                },
            );
        }
    };

    let mut status = None;
    let supervision = loop {
        let cpu = match live_cpu_usage_usec(&cgroup) {
            Ok(cpu) => cpu,
            Err(detail) => break Err(detail),
        };
        if cpu >= limits.cpu_usec {
            break Ok(Some(StopCause::CpuTimeout));
        }
        let signal = RECEIVED_SIGNAL.load(Ordering::SeqCst);
        if signal != 0 {
            break Ok(Some(StopCause::Cancelled(signal)));
        }
        let wall = match elapsed_ms(started) {
            Ok(wall) => wall,
            Err(detail) => break Err(detail),
        };
        if wall >= limits.wall_ms {
            break Ok(Some(StopCause::WallTimeout));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(observed) => status = observed,
                Err(error) => break Err(format!("cannot poll test command: {error}")),
            }
        }
        let group_status = match cgroup.status() {
            Ok(group_status) => group_status,
            Err(error) => break Err(format!("cannot read per-attempt cgroup status: {error}")),
        };
        if status.is_some() && group_status == ManualCpuCgroupStatus::Empty {
            break Ok(None);
        }
        thread::sleep(limits.poll);
    };

    let mut cause = match supervision {
        Ok(cause) => cause,
        Err(detail) => {
            let detail = abort_and_reap(&mut child, &cgroup, &mut status, detail);
            let completion = status.map(completion_from_status).transpose()?;
            return write_infrastructure_record(
                &record_dir,
                run_id,
                identity,
                limits,
                started,
                InfrastructureFailure {
                    completion,
                    cpu_usage_usec: None,
                    detail,
                },
            );
        }
    };

    if cause.is_some() {
        if let Err(detail) = reap_and_empty(&mut child, &cgroup, &mut status, true) {
            let detail = abort_and_reap(&mut child, &cgroup, &mut status, detail);
            let completion = status.map(completion_from_status).transpose()?;
            return write_infrastructure_record(
                &record_dir,
                run_id,
                identity,
                limits,
                started,
                InfrastructureFailure {
                    completion,
                    cpu_usage_usec: None,
                    detail,
                },
            );
        }
    }
    let cpu_used = match cgroup.final_cpu_usage_usec() {
        Ok(cpu) => cpu,
        Err(error) => {
            let detail = abort_and_reap(
                &mut child,
                &cgroup,
                &mut status,
                format!("cannot finalize per-attempt CPU accounting: {error}"),
            );
            let completion = status.map(completion_from_status).transpose()?;
            return write_infrastructure_record(
                &record_dir,
                run_id,
                identity,
                limits,
                started,
                InfrastructureFailure {
                    completion,
                    cpu_usage_usec: None,
                    detail,
                },
            );
        }
    };
    let wall_time_ms = elapsed_ms(started)?;
    if cpu_used >= limits.cpu_usec {
        cause = Some(StopCause::CpuTimeout);
    } else if cause.is_none() {
        let signal = RECEIVED_SIGNAL.load(Ordering::SeqCst);
        cause = if signal != 0 {
            Some(StopCause::Cancelled(signal))
        } else if wall_time_ms >= limits.wall_ms {
            Some(StopCause::WallTimeout)
        } else {
            None
        };
    }
    if let Err(error) = cgroup.cleanup() {
        let detail = abort_and_reap(
            &mut child,
            &cgroup,
            &mut status,
            format!("cannot remove finalized per-attempt cgroup: {error}"),
        );
        let completion = status.map(completion_from_status).transpose()?;
        return write_infrastructure_record(
            &record_dir,
            run_id,
            identity,
            limits,
            started,
            InfrastructureFailure {
                completion,
                cpu_usage_usec: Some(cpu_used),
                detail,
            },
        );
    }
    let status =
        status.ok_or_else(|| "child status is missing after cgroup became empty".to_string())?;
    let completion = completion_from_status(status)?;
    let outcome = match cause {
        None => AttemptOutcome::Completed,
        Some(StopCause::CpuTimeout) => AttemptOutcome::CpuTimeout,
        Some(StopCause::WallTimeout) => AttemptOutcome::WallTimeout,
        Some(StopCause::Cancelled(signal)) => AttemptOutcome::Cancelled { signal },
    };
    let record = AttemptRecord::new(
        run_id,
        identity,
        hermit_manifest_plan::nextest_cpu::AttemptMeasurement {
            cpu_usage_usec: Some(cpu_used),
            cpu_limit_usec: limits.cpu_usec,
            wall_time_ms,
            wall_limit_ms: limits.wall_ms,
        },
        Some(completion.clone()),
        outcome,
    );
    write_attempt_atomic(&record_dir, &record)?;

    match cause {
        Some(StopCause::CpuTimeout) => {
            eprintln!(
                "nextest-cpu-wrapper: CPU timeout after {cpu_used}us (limit {}us)",
                limits.cpu_usec
            );
            Ok(ExitStatus::from_raw((TIMEOUT_EXIT as i32) << 8))
        }
        Some(StopCause::WallTimeout) => {
            eprintln!(
                "nextest-cpu-wrapper: wall timeout after {wall_time_ms}ms (limit {}ms)",
                limits.wall_ms
            );
            Ok(ExitStatus::from_raw((TIMEOUT_EXIT as i32) << 8))
        }
        Some(StopCause::Cancelled(signal)) => propagate_signal(signal),
        None => match completion {
            AttemptCompletion::Exit { .. } => Ok(status),
            AttemptCompletion::Signal { signal } => propagate_signal(signal),
        },
    }
}

fn burn_cpu(milliseconds: u64) {
    let mut started = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut started);
    }
    let target_ns = milliseconds.saturating_mul(1_000_000) as i128;
    let mut value = 1u64;
    loop {
        value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
        std::hint::black_box(value);
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now);
        }
        let elapsed = (now.tv_sec - started.tv_sec) as i128 * 1_000_000_000
            + (now.tv_nsec - started.tv_nsec) as i128;
        if elapsed >= target_ns {
            break;
        }
    }
}

fn control_child(mode: &str, args: &[OsString]) -> Result<ExitCode, String> {
    match mode {
        "success" | "failure" => {
            let expected = ["--exact", mode, "--nocapture"];
            if args
                .iter()
                .map(OsString::as_os_str)
                .ne(expected.iter().map(OsStr::new))
            {
                return Err(format!(
                    "control child received changed arguments: {args:?}"
                ));
            }
            let expected_cwd = PathBuf::from(required_env(CONTROL_CWD_ENV)?);
            if env::current_dir().map_err(|error| error.to_string())? != expected_cwd {
                return Err("control child received a changed working directory".into());
            }
            if required_env(CONTROL_SENTINEL_ENV)? != "preserved" {
                return Err("control child received a changed environment".into());
            }
            if env::var_os(CPU_RECORD_DIR_ENV).is_some()
                || env::var_os(CPU_BINARY_MAP_ENV).is_some()
            {
                return Err(
                    "measurement-only configuration leaked into the test environment".into(),
                );
            }
            println!("stdout-exact");
            eprintln!("stderr-exact");
            Ok(ExitCode::from(if mode == "success" { 0 } else { 23 }))
        }
        "signal" => unsafe {
            libc::raise(libc::SIGUSR1);
            libc::_exit(255);
        },
        "cpu-live" => {
            burn_cpu(500);
            Ok(ExitCode::SUCCESS)
        }
        "cpu-fast" => {
            burn_cpu(80);
            Ok(ExitCode::SUCCESS)
        }
        "wall" | "accounting-error" => {
            thread::sleep(Duration::from_millis(400));
            Ok(ExitCode::SUCCESS)
        }
        "peer-target" => {
            burn_cpu(100);
            Ok(ExitCode::SUCCESS)
        }
        "peer-burn" => {
            burn_cpu(500);
            Ok(ExitCode::SUCCESS)
        }
        "tree" => {
            let executable = env::current_exe().map_err(|error| error.to_string())?;
            let mut children = Vec::new();
            for _ in 0..2 {
                children.push(
                    Command::new(&executable)
                        .args(["--exact", "burn", "--nocapture"])
                        .env(CONTROL_ARM_ENV, "1")
                        .spawn()
                        .map_err(|error| format!("cannot spawn CPU child: {error}"))?,
                );
            }
            for mut child in children {
                let status = child
                    .wait()
                    .map_err(|error| format!("cannot wait for CPU child: {error}"))?;
                if !status.success() {
                    return Err(format!("CPU child failed with {status}"));
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        "burn" => {
            burn_cpu(100);
            Ok(ExitCode::SUCCESS)
        }
        "external-cancel" => {
            let path = PathBuf::from(required_env(CONTROL_PID_FILE_ENV)?);
            fs::write(&path, format!("{}\n", std::process::id()))
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        "ignore-term" => {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = libc::SIG_IGN;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
                    return Err(format!(
                        "cannot ignore SIGTERM: {}",
                        io::Error::last_os_error()
                    ));
                }
                if libc::setsid() < 0 {
                    return Err(format!(
                        "cannot create escaped session: {}",
                        io::Error::last_os_error()
                    ));
                }
            }
            let path = PathBuf::from(required_env(CONTROL_PID_FILE_ENV)?);
            fs::write(&path, format!("{}\n", std::process::id()))
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }
        _ => Err(format!("unknown control-child mode {mode:?}")),
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let path = env::temp_dir().join(format!(
            "hermit-nextest-cpu-wrapper-self-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("attempts"))
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn control_command(
    executable: &Path,
    test_binary: &Path,
    scratch: &Path,
    mode: &str,
    attempt: u64,
) -> Command {
    let mut command = Command::new(executable);
    command
        .arg(test_binary)
        .args(["--exact", mode, "--nocapture"])
        .current_dir(scratch)
        .env(CPU_BINARY_MAP_ENV, scratch.join("binary-map.json"))
        .env(CPU_RECORD_DIR_ENV, scratch.join("attempts"))
        .env(RUN_ID_ENV, "self-test-run")
        .env(PACKAGE_ENV, "fixture")
        .env(PRIVATE_ATTEMPT_ENV, attempt.to_string())
        .env(CONTROL_ARM_ENV, "1")
        .env(CONTROL_CWD_ENV, scratch)
        .env(CONTROL_SENTINEL_ENV, "preserved")
        .env(CONTROL_CPU_LIMIT_USEC_ENV, "2000000")
        .env(CONTROL_WALL_LIMIT_MS_ENV, "5000")
        .env(CONTROL_POLL_MS_ENV, "10")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

fn find_record<'a>(records: &'a [AttemptRecord], test: &str) -> Result<&'a AttemptRecord, String> {
    records
        .iter()
        .find(|record| record.identity.test == test)
        .ok_or_else(|| format!("self-test did not find the {test:?} attempt record"))
}

fn pin_current_to_first_allowed_cpu() -> Result<libc::cpu_set_t, String> {
    let mut original = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    let size = std::mem::size_of::<libc::cpu_set_t>();
    if unsafe { libc::sched_getaffinity(0, size, &mut original) } != 0 {
        return Err(format!(
            "cannot read self-test CPU affinity: {}",
            io::Error::last_os_error()
        ));
    }
    let cpu = (0..libc::CPU_SETSIZE as usize)
        .find(|cpu| unsafe { libc::CPU_ISSET(*cpu, &original) })
        .ok_or_else(|| "self-test CPU affinity contains no CPU".to_string())?;
    let mut one = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut one);
        libc::CPU_SET(cpu, &mut one);
    }
    if unsafe { libc::sched_setaffinity(0, size, &one) } != 0 {
        return Err(format!(
            "cannot pin self-test children: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(original)
}

fn restore_affinity(original: &libc::cpu_set_t) -> Result<(), String> {
    if unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), original) } != 0
    {
        return Err(format!(
            "cannot restore self-test CPU affinity: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn self_test() -> Result<(), String> {
    let scratch = Scratch::new()?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let test_binary = scratch.0.join("fixture_name-0123456789abcdef");
    std::os::unix::fs::symlink(&executable, &test_binary)
        .map_err(|error| format!("cannot link self-test binary: {error}"))?;
    let map = BinaryMap {
        schema: BINARY_MAP_SCHEMA,
        entries: vec![BinaryMapEntry {
            executable: test_binary
                .to_str()
                .ok_or_else(|| "self-test path is not UTF-8".to_string())?
                .into(),
            package: "fixture".into(),
            binary: "fixture::bin/fixture_name".into(),
            binary_name: "fixture_name".into(),
            kind: "bin".into(),
        }],
    };
    write_binary_map_atomic(&scratch.0.join("binary-map.json"), &map)?;

    let success = control_command(&executable, &test_binary, &scratch.0, "success", 1)
        .output()
        .map_err(|error| format!("cannot run success control: {error}"))?;
    if !success.status.success()
        || success.stdout != b"stdout-exact\n"
        || success.stderr != b"stderr-exact\n"
    {
        return Err(format!(
            "success control changed observable behavior: {success:?}"
        ));
    }

    let failure = control_command(&executable, &test_binary, &scratch.0, "failure", 1)
        .output()
        .map_err(|error| format!("cannot run failure control: {error}"))?;
    if failure.status.code() != Some(23)
        || failure.stdout != b"stdout-exact\n"
        || failure.stderr != b"stderr-exact\n"
    {
        return Err(format!(
            "failure control changed observable behavior: {failure:?}"
        ));
    }

    let signal = control_command(&executable, &test_binary, &scratch.0, "signal", 1)
        .output()
        .map_err(|error| format!("cannot run signal control: {error}"))?;
    if signal.status.signal() != Some(libc::SIGUSR1)
        || !signal.stdout.is_empty()
        || !signal.stderr.is_empty()
    {
        return Err(format!(
            "signal control changed observable behavior: {signal:?}"
        ));
    }

    let tree = control_command(&executable, &test_binary, &scratch.0, "tree", 1)
        .output()
        .map_err(|error| format!("cannot run process-tree control: {error}"))?;
    if !tree.status.success() {
        return Err(format!("process-tree control failed: {tree:?}"));
    }

    let substituted_binary = scratch.0.join("substituted-0123456789abcdef");
    std::os::unix::fs::symlink(&executable, &substituted_binary)
        .map_err(|error| format!("cannot link substituted self-test binary: {error}"))?;
    let substituted = control_command(&executable, &substituted_binary, &scratch.0, "success", 1)
        .output()
        .map_err(|error| format!("cannot run substituted-path control: {error}"))?;
    if substituted.status.code() != Some(INFRASTRUCTURE_EXIT.into())
        || !String::from_utf8_lossy(&substituted.stderr).contains("absent from the typed inventory")
    {
        return Err(format!(
            "substituted-path control was not refused: {substituted:?}"
        ));
    }

    let pid_file = scratch.0.join("external-cancel-child.pid");
    let mut wall_command =
        control_command(&executable, &test_binary, &scratch.0, "external-cancel", 1);
    wall_command.env(CONTROL_PID_FILE_ENV, &pid_file);
    let wall_child = wall_command
        .spawn()
        .map_err(|error| format!("cannot run wall-timeout control: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !pid_file.is_file() {
        return Err("wall-timeout control child did not start".into());
    }
    if unsafe { libc::kill(-(wall_child.id() as i32), libc::SIGTERM) } != 0 {
        return Err(format!(
            "cannot signal wall-timeout process group: {}",
            io::Error::last_os_error()
        ));
    }
    let wall = wall_child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for wall-timeout control: {error}"))?;
    if wall.status.signal() != Some(libc::SIGTERM) {
        return Err(format!(
            "wall-timeout control did not preserve SIGTERM: {:?}",
            wall.status
        ));
    }

    let mut cpu_live_command =
        control_command(&executable, &test_binary, &scratch.0, "cpu-live", 1);
    cpu_live_command.env(CONTROL_CPU_LIMIT_USEC_ENV, "50000");
    let cpu_live = cpu_live_command
        .output()
        .map_err(|error| format!("cannot run live CPU-timeout control: {error}"))?;
    if cpu_live.status.code() != Some(TIMEOUT_EXIT.into())
        || !String::from_utf8_lossy(&cpu_live.stderr).contains("CPU timeout")
    {
        return Err(format!(
            "live CPU-timeout control did not time out: {cpu_live:?}"
        ));
    }

    let mut cpu_fast_command =
        control_command(&executable, &test_binary, &scratch.0, "cpu-fast", 1);
    cpu_fast_command
        .env(CONTROL_CPU_LIMIT_USEC_ENV, "20000")
        .env(CONTROL_POLL_MS_ENV, "250");
    let cpu_fast = cpu_fast_command
        .output()
        .map_err(|error| format!("cannot run fast-exit CPU-timeout control: {error}"))?;
    if cpu_fast.status.code() != Some(TIMEOUT_EXIT.into()) {
        return Err(format!(
            "fast-exit CPU-timeout control did not time out: {cpu_fast:?}"
        ));
    }

    let mut wall_timeout_command =
        control_command(&executable, &test_binary, &scratch.0, "wall", 1);
    wall_timeout_command.env(CONTROL_WALL_LIMIT_MS_ENV, "100");
    let wall_timeout = wall_timeout_command
        .output()
        .map_err(|error| format!("cannot run wall-timeout control: {error}"))?;
    if wall_timeout.status.code() != Some(TIMEOUT_EXIT.into())
        || !String::from_utf8_lossy(&wall_timeout.stderr).contains("wall timeout")
    {
        return Err(format!(
            "wall-timeout control did not time out: {wall_timeout:?}"
        ));
    }

    let mut accounting =
        control_command(&executable, &test_binary, &scratch.0, "accounting-error", 1);
    accounting.env(CONTROL_ACCOUNTING_ERROR_ENV, "1");
    let accounting = accounting
        .output()
        .map_err(|error| format!("cannot run accounting-error control: {error}"))?;
    if accounting.status.code() != Some(INFRASTRUCTURE_EXIT.into())
        || !String::from_utf8_lossy(&accounting.stderr).contains("malformed CPU accounting")
    {
        return Err(format!(
            "accounting-error control did not refuse: {accounting:?}"
        ));
    }

    let escaped_pid_file = scratch.0.join("ignore-term-child.pid");
    let mut escaped_command =
        control_command(&executable, &test_binary, &scratch.0, "ignore-term", 1);
    escaped_command
        .env(CONTROL_PID_FILE_ENV, &escaped_pid_file)
        .env(CONTROL_WALL_LIMIT_MS_ENV, "100");
    let escaped = escaped_command
        .output()
        .map_err(|error| format!("cannot run escaped-descendant control: {error}"))?;
    if escaped.status.code() != Some(TIMEOUT_EXIT.into()) || !escaped_pid_file.is_file() {
        return Err(format!(
            "escaped-descendant control did not time out: {escaped:?}"
        ));
    }
    let escaped_pid = fs::read_to_string(&escaped_pid_file)
        .map_err(|error| format!("cannot read escaped pid: {error}"))?
        .trim()
        .parse::<i32>()
        .map_err(|error| format!("invalid escaped pid: {error}"))?;
    if unsafe { libc::kill(escaped_pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err(format!(
            "escaped descendant pid {escaped_pid} survived cgroup kill"
        ));
    }

    // The affinity applies only to this mutation control and its children, never to an ordinary
    // validate. The unrelated peer consumes the same CPU while remaining outside the attempt's
    // cgroup. The target must reach its wall bound with less than its CPU allowance consumed.
    let original_affinity = pin_current_to_first_allowed_cpu()?;
    let mut peer = Command::new(&executable)
        .args(["--exact", "peer-burn", "--nocapture"])
        .env(CONTROL_ARM_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot spawn peer-descheduling control: {error}"))?;
    let mut peer_target = control_command(&executable, &test_binary, &scratch.0, "peer-target", 1);
    peer_target
        .env(CONTROL_CPU_LIMIT_USEC_ENV, "150000")
        .env(CONTROL_WALL_LIMIT_MS_ENV, "120");
    let peer_target = peer_target
        .spawn()
        .map_err(|error| format!("cannot spawn peer-descheduled target: {error}"))?;
    restore_affinity(&original_affinity)?;
    let peer_target = peer_target
        .wait_with_output()
        .map_err(|error| format!("cannot wait for peer-descheduled target: {error}"))?;
    let peer_status = peer
        .wait()
        .map_err(|error| format!("cannot wait for peer-descheduling control: {error}"))?;
    if !peer_status.success() || peer_target.status.code() != Some(TIMEOUT_EXIT.into()) {
        return Err(format!(
            "peer-descheduling control did not preserve distinct wall enforcement: peer={peer_status}, target={peer_target:?}"
        ));
    }

    let records = read_attempt_records(&scratch.0.join("attempts"))?;
    if records.len() != 11 {
        return Err(format!(
            "self-test expected eleven atomic attempt records, found {}",
            records.len()
        ));
    }
    if records
        .iter()
        .any(|record| record.identity.binary != "fixture::bin/fixture_name")
    {
        return Err("self-test did not preserve the typed binary identity".into());
    }
    if !matches!(
        find_record(&records, "success")?.completion,
        Some(AttemptCompletion::Exit { code: 0 })
    ) || !matches!(
        find_record(&records, "failure")?.completion,
        Some(AttemptCompletion::Exit { code: 23 })
    ) || !matches!(
        find_record(&records, "signal")?.completion,
        Some(AttemptCompletion::Signal {
            signal: libc::SIGUSR1
        })
    ) || !matches!(
        find_record(&records, "external-cancel")?.outcome,
        AttemptOutcome::Cancelled {
            signal: libc::SIGTERM
        }
    ) {
        return Err(
            "self-test attempt completion records do not preserve exit/signal status".into(),
        );
    }
    let tree_record = find_record(&records, "tree")?;
    if tree_record.cpu_usage_usec.unwrap_or(0) < 150_000 {
        return Err(format!(
            "process-tree control expected at least 150000us, measured {:?}us",
            tree_record.cpu_usage_usec
        ));
    }
    if !matches!(
        find_record(&records, "cpu-live")?.outcome,
        AttemptOutcome::CpuTimeout
    ) || !matches!(
        find_record(&records, "cpu-fast")?.outcome,
        AttemptOutcome::CpuTimeout
    ) || !matches!(
        find_record(&records, "wall")?.outcome,
        AttemptOutcome::WallTimeout
    ) || !matches!(
        find_record(&records, "accounting-error")?.outcome,
        AttemptOutcome::InfrastructureError { .. }
    ) || !matches!(
        find_record(&records, "ignore-term")?.outcome,
        AttemptOutcome::WallTimeout
    ) {
        return Err("self-test attempt outcomes do not preserve their distinct causes".into());
    }
    let fast = find_record(&records, "cpu-fast")?;
    if !matches!(fast.completion, Some(AttemptCompletion::Exit { code: 0 }))
        || fast.cpu_usage_usec.unwrap_or(0) < fast.cpu_limit_usec
    {
        return Err(
            "fast-exit control was not classified from retained final CPU accounting".into(),
        );
    }
    if find_record(&records, "accounting-error")?
        .cpu_usage_usec
        .is_some()
    {
        return Err("malformed accounting control fabricated a CPU value".into());
    }
    let peer_target = find_record(&records, "peer-target")?;
    if !matches!(peer_target.outcome, AttemptOutcome::WallTimeout)
        || peer_target.cpu_usage_usec.unwrap_or(u64::MAX) >= peer_target.cpu_limit_usec
    {
        return Err(format!(
            "peer-descheduled target did not stop on wall below its CPU limit: {peer_target:?}"
        ));
    }
    let duplicate = write_attempt_atomic(&scratch.0.join("attempts"), &records[0]);
    if duplicate.is_ok() {
        return Err("duplicate atomic attempt publication unexpectedly replaced a record".into());
    }
    println!(
        "nextest-cpu-wrapper: self-test PASS (success, failure, signal, cancellation, live and fast CPU timeout, wall timeout, peer descheduling, malformed accounting, escaped descendant, process tree, typed identity, substituted path, atomic identity)"
    );
    Ok(())
}

fn main() -> ExitCode {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--self-test") {
        return match self_test() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("nextest-cpu-wrapper: {error}");
                ExitCode::from(INFRASTRUCTURE_EXIT)
            }
        };
    }
    if env::var_os(CONTROL_ARM_ENV).is_some() && env::var_os(CPU_RECORD_DIR_ENV).is_none() {
        let mode = args
            .windows(2)
            .find(|window| window[0] == "--exact")
            .and_then(|window| window[1].to_str());
        return match mode {
            Some(mode) => control_child(mode, &args).unwrap_or_else(|error| {
                eprintln!("nextest-cpu-wrapper control: {error}");
                ExitCode::from(INFRASTRUCTURE_EXIT)
            }),
            None => ExitCode::from(INFRASTRUCTURE_EXIT),
        };
    }
    match run_wrapper(args) {
        Ok(status) => ExitCode::from(status.code().unwrap_or(INFRASTRUCTURE_EXIT as i32) as u8),
        Err(error) => {
            eprintln!("nextest-cpu-wrapper: {error}");
            ExitCode::from(INFRASTRUCTURE_EXIT)
        }
    }
}
