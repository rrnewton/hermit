/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Host-only controls entered through the maintained ordinary-main lifecycle
//! role. There is no guest, no backend opt-in, and no libtest startup assertion.
//! The original three-second predicate and separate two-second cleanup remain.
use std::cell::Cell;
use std::cell::RefCell;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use hermit::Backend;
use hermit::Error;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Output;

use super::cli_owned_lifecycle::pidfd;
use super::cli_owned_lifecycle::ready;
use super::cli_owned_lifecycle::receive_fd;
use super::cli_owned_lifecycle::transfer_fd;
use super::cli_owned_lifecycle::wait;
use super::native_exit;
use super::owned_container;
use super::verify;

#[derive(Debug)]
struct DropWitness {
    original: libc::pid_t,
    path: PathBuf,
    label: &'static str,
}
impl DropWitness {
    fn new(directory: &Path, label: &'static str) -> Self {
        Self {
            original: unsafe { libc::getpid() },
            path: directory.join("drops"),
            label,
        }
    }
}
impl Drop for DropWitness {
    fn drop(&mut self) {
        // Container children copy these values; only original-main destruction
        // is the predicate. This is not an owner-field-destructor detector.
        if unsafe { libc::getpid() } == self.original {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .unwrap();
            writeln!(file, "{}", self.label).unwrap();
        }
    }
}
#[derive(Debug)]
struct OriginalFailure {
    retained: Error,
    token: u64,
    _drop: DropWitness,
}
impl std::fmt::Display for OriginalFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "ORIGINAL_FATAL_FIXTURE_PRIMARY {}: {:#}",
            self.token, self.retained
        )
    }
}
impl std::error::Error for OriginalFailure {}

fn send_signal(fd: RawFd, signal: i32) -> std::io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn wait_record(fd: RawFd, options: i32) -> std::io::Result<Option<(i32, i32, i32)>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::waitid(libc::P_PIDFD, fd as u32, &mut info, options | libc::__WALL) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pid = unsafe { info.si_pid() };
    Ok((pid != 0).then(|| (pid, info.si_code, unsafe { info.si_status() })))
}
fn channel_ready(fd: RawFd) -> bool {
    let mut event = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut event, 1, 0) };
    assert!(rc >= 0, "fixture channel poll failed");
    rc > 0 && event.revents & libc::POLLIN != 0
}

fn buffer_real_comparison(
    directory: &Path,
    destination: &Path,
    announce: bool,
) -> Result<(), Error> {
    // Real comparator and typed report, with explicitly host-fixture operands.
    // These bytes are never evidence of a guest run or of the parent-death cell.
    let suffix = detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Syscall);
    let log = [1, 2].map(|value| format!(
        "2026-08-06T01:00:00.000000Z INFO detcore: [dtid 2] DETLOG [syscall] write(fd=1, count={value}){suffix}\n"
    )).concat();
    let mut first = tempfile::NamedTempFile::new_in(directory)?;
    let mut second = tempfile::NamedTempFile::new_in(directory)?;
    first.write_all(log.as_bytes())?;
    second.write_all(log.as_bytes())?;
    let output = Output {
        status: ExitStatus::Exited(0),
        stdout: b"host lifecycle fixture\n".to_vec(),
        stderr: Vec::new(),
    };
    let outcome = verify::compare_two_runs(
        verify::ComparedRun {
            output: &output,
            log: first.into_temp_path(),
            label: "host fixture first",
        },
        verify::ComparedRun {
            output: &output,
            log: second.into_temp_path(),
            label: "host fixture second",
        },
        verify::ComparisonOptions {
            verbose: false,
            strictness: verify::LogCompareStrictness::Canonical,
            compare_logs: true,
            diagnostic_full_trace: false,
            compare_io_buffers: true,
            keep_logs: false,
            match_overridden: false,
            failed_log_retention: None,
            record_envelope: super::record_envelope::RecordEnvelope::all_records_v1(),
            virtualize_time: true,
        },
    )?;
    let report = verify::verification_report(&outcome);
    anyhow::ensure!(report.verified, "host fixture comparison did not match");
    fs::write(
        directory.join("expected-verified.json"),
        format!("{}\n", serde_json::to_string(&report)?),
    )?;
    verify::write_report_json(destination, &report)?;
    if announce {
        const SUCCESS: &str = "Success: deterministic. Determinism verified.";
        let mut expected = Vec::new();
        verify::write_verification_announcement(
            &mut expected,
            &outcome,
            verify::SecondRun::Rerun,
            SUCCESS,
            "Failure: nondeterministic.",
        )?;
        fs::write(directory.join("expected-announcement"), &expected)?;
        fs::write(
            directory.join("stderr-before-announcement"),
            fs::read(directory.join("stderr"))?,
        )?;
        // Exercise the actual existing entrypoint, also on the old test-only
        // target where this API returns (). The assertion is made on real
        // captured bytes after the existing owned cleanup, never on timeout.
        let _ = verify::announce_verification_outcome(
            &outcome,
            verify::SecondRun::Rerun,
            SUCCESS,
            "Failure: nondeterministic.",
        );
        fs::write(
            directory.join("stderr-before-settlement"),
            fs::read(directory.join("stderr"))?,
        )?;
    }
    Ok(())
}

fn invocation(mode: &str, deadline: Instant, directory: &Path) -> i32 {
    let announce = mode.ends_with("-announcement");
    let mode = mode.strip_suffix("-announcement").unwrap_or(mode);
    let settled = mode == "native-exit-child-settled";
    let panic_reporter = mode == "native-exit-child-fatal-panic";
    let successful_work = mode == "native-exit-child-fatal-success";
    assert!(settled || panic_reporter || successful_work || mode == "native-exit-child-fatal-io");
    let destination = directory.join("verify.json");
    let deferred = verify::DeferredVerification::begin(&destination).unwrap();
    let pending = fs::read(&destination).unwrap();
    assert_eq!(
        pending,
        format!(
            "{}\n",
            serde_json::to_string(&verify::VerificationReport::no_result()).unwrap()
        )
        .as_bytes()
    );
    fs::write(directory.join("expected-no-result.json"), &pending).unwrap();
    let evidence = RefCell::new(None);
    let normal = Cell::new(false);
    let result = native_exit::with_early_owner_reporting(
        true,
        |owner| -> Result<DropWitness, Error> {
            let owner = owner.ok_or_else(|| {
                anyhow::anyhow!("fixture requires real early-main broker capability")
            })?;
            let client = owner.client();
            let identity = client
                .authenticated_broker_identity()
                .map_err(|error| anyhow::anyhow!("authenticated fixture broker: {error:?}"))?;
            fs::write(directory.join("broker-pid"), identity.pid().to_string())?;
            transfer_fd(libc::STDIN_FILENO, identity.pidfd().as_raw_fd());
            if !settled {
                // Pin an actual stopped broker while selecting the fatal branch.
                // No job exists. This makes subsequent PDEATHSIG/actual wait causal,
                // and exposes an erroneous blind shutdown before the fatal decision.
                send_signal(identity.pidfd().as_raw_fd(), libc::SIGSTOP)?;
                let mut stopped = None;
                anyhow::ensure!(
                    wait(deadline, || {
                        stopped = wait_record(
                            identity.pidfd().as_raw_fd(),
                            libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
                        )
                        .unwrap();
                        stopped.is_some()
                    }),
                    "fixture broker did not stop before original deadline"
                );
                anyhow::ensure!(
                    stopped == Some((identity.pid(), libc::CLD_STOPPED, libc::SIGSTOP)),
                    "unexpected broker stop record"
                );
                fs::write(
                    directory.join("broker-stopped.json"),
                    serde_json::to_vec(&stopped)?,
                )?;
            }
            buffer_real_comparison(directory, &destination, announce)?;
            anyhow::ensure!(
                fs::read(&destination)? == pending,
                "pending comparison leaked before native settlement"
            );
            if !settled {
                let evidence_path = directory.join("rich-evidence");
                *evidence.borrow_mut() = Some(super::run_evidence::RunEvidenceSession::create(
                    &evidence_path,
                    Backend::Kvm,
                )?);
                // Actual no-replace publication must fail even when tests run as
                // root. The verification destination remains writable and distinct.
                fs::write(
                    evidence_path.join(hermit::run_evidence::RUN_EVIDENCE_MANIFEST),
                    b"fixture occupied destination\n",
                )?;
            }
            let mut container = Container::new();
            let result: Result<(u64, DropWitness), Error> = owned_container::run(
                &mut container,
                DropWitness::new(directory, "factory"),
                "native-exit host lifecycle backing".to_owned(),
                false,
                "native-exit-lifecycle",
                None,
                move |_| {
                    if settled {
                        Ok(41)
                    } else {
                        Err(anyhow::anyhow!("REPORTED_CONTAINER_FIXTURE_FAILURE"))
                    }
                },
            );
            if settled {
                let (value, guard) = result?;
                anyhow::ensure!(
                    value == 41 && !owned_container::has_retained_owner(),
                    "normal container result changed"
                );
                drop(guard);
                return Ok(DropWitness::new(directory, "result"));
            }
            let error = match result {
                Err(error) => error,
                Ok(_) => anyhow::bail!("reported-error fixture returned success"),
            };
            let reaped =
                owned_container::lifecycle_retained_native_wait(&error).ok_or_else(|| {
                    anyhow::anyhow!("fixture did not retain its actual waited container factory")
                })?;
            anyhow::ensure!(
                reaped.0 > 0
                    && reaped.1 == ExitStatus::Exited(0)
                    && owned_container::has_retained_owner(),
                "fixture factory retention/native wait mismatch"
            );
            fs::write(
                directory.join("container-wait.json"),
                serde_json::to_vec(&serde_json::json!({
                    "pid": reaped.0, "status": "Exited(0)", "source": "actual owned_container retained observation"
                }))?,
            )?;
            if successful_work {
                // A buffered successful result still cannot outrun a retained
                // factory. Preserve the real primary from the child separately.
                let _retained = ManuallyDrop::new(error);
                Ok(DropWitness::new(directory, "result"))
            } else {
                Err(Error::new(OriginalFailure {
                    retained: error,
                    token: 371,
                    _drop: DropWitness::new(directory, "primary"),
                }))
            }
        },
        |fatal| {
            if successful_work {
                assert!(fatal.primary.is_none());
            } else {
                assert_eq!(
                    fatal
                        .primary
                        .unwrap()
                        .downcast_ref::<OriginalFailure>()
                        .unwrap()
                        .token,
                    371
                );
            }
            assert!(owned_container::has_retained_owner());
            fs::write(
                directory.join("reporter-entered"),
                b"original primary/result and retained factory observed\n",
            )
            .unwrap();
            super::report_fatal_native_invocation(&evidence, fatal);
            fs::write(
                directory.join("reporter-returned"),
                b"production reporter returned after real evidence error\n",
            )
            .unwrap();
            if panic_reporter {
                eprintln!("HOST_LIFECYCLE_REPORTER_PANIC");
                std::panic::panic_any(DropWitness::new(directory, "reporter-panic"));
            }
        },
        || {
            normal.set(true);
            fs::write(
                directory.join("normally-settled"),
                b"actual wrapper settlement callback\n",
            )
            .unwrap();
        },
    );
    // This is the ordinary production sequence, not a synthetic publication.
    deferred.finish(normal.get(), result.is_ok()).unwrap();
    assert!(settled, "fatal invocation returned instead of aborting");
    drop(result.unwrap());
    assert!(normal.get());
    assert_eq!(
        fs::read(&destination).unwrap(),
        fs::read(directory.join("expected-verified.json")).unwrap()
    );
    0
}

fn supervisor(mode: &str, deadline: Instant, end: u64) -> i32 {
    let requested_mode = mode;
    let announce = mode.ends_with("-announcement");
    let mode = mode.strip_suffix("-announcement").unwrap_or(mode);
    let settled = mode == "native-exit-settled";
    assert!(
        settled
            || matches!(
                mode,
                "native-exit-fatal-io" | "native-exit-fatal-panic" | "native-exit-fatal-success"
            )
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let (channel, sender) = std::os::unix::net::UnixDatagram::pair().unwrap();
    let parent = unsafe { libc::getpid() };
    let child_mode = requested_mode.replacen("native-exit-", "native-exit-child-", 1);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env("HERMIT_INTERNAL_CLI_LIFECYCLE", "1")
        .args([
            "__hermit-cli-lifecycle",
            &child_mode,
            &end.to_string(),
            path.to_str().unwrap(),
        ])
        .stdin(Stdio::from(OwnedFd::from(sender)))
        .stdout(File::create(path.join("stdout")).unwrap())
        .stderr(File::create(path.join("stderr")).unwrap());
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);
    let cli = pidfd(child.id() as i32);
    let mut broker = None;
    let mut status = None;
    // Catch fixture assertion failures so all acquired native owners reach the
    // same separate cleanup before the original failed predicate is returned.
    let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            wait(deadline, || {
                if channel_ready(channel.as_raw_fd()) {
                    broker = Some(receive_fd(channel.as_raw_fd()));
                    return true;
                }
                status = child.try_wait().unwrap();
                status.is_some()
            }),
            "neither authenticated broker identity nor child result arrived"
        );
        assert!(broker.is_some(), "real broker setup failed");
        assert!(
            wait(deadline, || {
                status = child.try_wait().unwrap();
                status.is_some()
            }),
            "original three-second invocation predicate missed"
        );
        assert_eq!(status.unwrap().code(), Some(if settled { 0 } else { 125 }));
    }));
    let original_passed = observation.is_ok();
    let cleanup_start = Instant::now();
    let cleanup_deadline = cleanup_start + Duration::from_secs(2);
    // Reserve the latter half of this SAME two-second cleanup interval for
    // any necessary signal plus actual wait. This never extends the predicate.
    let broker_passive_deadline = cleanup_start + Duration::from_secs(1);
    let mut rescue_used = false;
    if status.is_none() {
        rescue_used = true;
        let _ = send_signal(cli.as_raw_fd(), libc::SIGKILL);
        let _ = wait(cleanup_deadline, || {
            status = child.try_wait().ok().flatten();
            status.is_some()
        });
    }
    // If a setup assertion won the channel race, retain the real transferred
    // pidfd now. The channel carries exactly one descriptor from the child.
    if broker.is_none() && channel_ready(channel.as_raw_fd()) {
        broker = Some(receive_fd(channel.as_raw_fd()));
    }
    let mut broker_wait = None;
    let mut broker_wait_errno = None;
    let mut broker_ready = false;
    if let Some(fd) = &broker {
        broker_ready = ready(fd) || wait(broker_passive_deadline, || ready(fd));
        if !broker_ready {
            rescue_used = true;
            let _ = send_signal(fd.as_raw_fd(), libc::SIGKILL);
            let _ = ready(fd) || wait(cleanup_deadline, || ready(fd));
        }
        let mut observe_wait = || match wait_record(fd.as_raw_fd(), libc::WEXITED | libc::WNOHANG) {
            Ok(record) => {
                broker_wait = record;
                record.is_some()
            }
            Err(error) => {
                broker_wait_errno = error.raw_os_error();
                true
            }
        };
        // Always perform a real nonblocking observation, even if earlier CLI
        // cleanup consumed the interval. Expiry is not an empty-owner receipt.
        if !observe_wait() && !wait(cleanup_deadline, &mut observe_wait) {
            let _ = observe_wait();
        }
        // The final nonblocking observation can see an exit at the deadline.
        // Recheck this same retained pidfd; do not invent readiness from time.
        broker_ready = ready(fd);
    }
    // Real kernel child census after exact waits. A live adopted child returns
    // zero/si_pid=0; it is not rewritten as an empty census or successful wait.
    let mut remaining: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let census_rc = unsafe {
        libc::waitid(
            libc::P_ALL,
            0,
            &mut remaining,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL,
        )
    };
    let census_errno = (census_rc < 0)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten();
    let cleanup_complete = status.is_some() && broker_ready && census_errno == Some(libc::ECHILD);
    let stdout = fs::read_to_string(path.join("stdout")).unwrap();
    let stderr = fs::read_to_string(path.join("stderr")).unwrap();
    print!("{stdout}");
    eprint!("{stderr}");
    println!(
        "{}",
        serde_json::json!({"phase":"host native-exit lifecycle actual cleanup",
        "mode":mode,"original_predicate":original_passed,"cli_status":status.map(|s|s.to_string()),
        "broker_wait":broker_wait,"broker_wait_errno":broker_wait_errno,"broker_ready":broker_ready,
        "remaining_waitid_rc":census_rc,"remaining_waitid_errno":census_errno,
        "remaining_pid":unsafe{remaining.si_pid()},"cleanup_complete":cleanup_complete,"rescue_used":rescue_used})
    );
    if !cleanup_complete {
        let preserved = directory.keep();
        eprintln!(
            "HOST_LIFECYCLE_UNRESOLVED: evidence retained at {}",
            preserved.display()
        );
        return 1;
    }
    // Every assertion below is after actual child/reaper cleanup. Neither the
    // separate rescue nor the outer five-second ceiling can satisfy success.
    assert!(
        original_passed && !rescue_used,
        "original predicate failed after owned cleanup"
    );
    let drops = match fs::read(path.join("drops")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("reading actual destructor evidence: {error}"),
    };
    if settled {
        assert_eq!(broker_wait_errno, Some(libc::ECHILD));
        assert!(broker_wait.is_none());
        assert!(path.join("normally-settled").is_file());
        assert_eq!(drops, b"factory\nresult\n");
        assert!(!path.join("reporter-entered").exists());
        assert_eq!(
            fs::read(path.join("verify.json")).unwrap(),
            fs::read(path.join("expected-verified.json")).unwrap()
        );
    } else {
        let expected_pid: i32 = fs::read_to_string(path.join("broker-pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            broker_wait,
            Some((expected_pid, libc::CLD_KILLED, libc::SIGKILL))
        );
        assert!(broker_wait_errno.is_none());
        assert!(path.join("broker-stopped.json").is_file());
        assert!(path.join("container-wait.json").is_file());
        assert!(
            path.join("reporter-entered").is_file() && path.join("reporter-returned").is_file()
        );
        assert!(!path.join("normally-settled").exists());
        assert!(
            drops.is_empty(),
            "retained original result/primary/factory/reporter payload was dropped"
        );
        assert_eq!(
            fs::read(path.join("verify.json")).unwrap(),
            fs::read(path.join("expected-no-result.json")).unwrap()
        );
        assert!(stderr.contains("HERMIT_FATAL_INVOCATION_ABORT: requested_exit=125"));
        assert!(stderr.contains("HERMIT_FATAL_EVIDENCE_FAILED:"));
        assert_eq!(
            stderr.contains("HOST_LIFECYCLE_REPORTER_PANIC"),
            mode == "native-exit-fatal-panic"
        );
        assert_eq!(
            fs::read(
                path.join("rich-evidence")
                    .join(hermit::run_evidence::RUN_EVIDENCE_MANIFEST)
            )
            .unwrap(),
            b"fixture occupied destination\n"
        );
    }
    if announce {
        let expected = fs::read(path.join("expected-announcement")).unwrap();
        let before = fs::read(path.join("stderr-before-announcement")).unwrap();
        let unsettled = fs::read(path.join("stderr-before-settlement")).unwrap();
        // Keep the observations explicit and assert only after actual child and
        // broker cleanup above. An outer timeout or rescue cannot satisfy this.
        println!(
            "{}",
            serde_json::json!({
                "phase": "verification announcement ordering after actual cleanup",
                "mode": requested_mode, "before_bytes": before.len(),
                "unsettled_bytes": unsettled.len(), "expected_bytes": expected.len(),
                "cleanup_complete": cleanup_complete,
            })
        );
        assert_eq!(
            unsettled, before,
            "console verdict escaped before native settlement"
        );
        if settled {
            let mut exact = before;
            exact.extend_from_slice(&expected);
            assert_eq!(
                stderr.as_bytes(),
                exact,
                "settled announcement bytes changed"
            );
        } else {
            assert!(
                !stderr
                    .as_bytes()
                    .windows(expected.len())
                    .any(|bytes| bytes == expected),
                "fatal invocation printed the buffered success verdict"
            );
        }
    }
    0
}

pub(super) fn run(mode: &str, deadline: Instant, end: u64, directory: Option<&str>) -> i32 {
    if mode.starts_with("native-exit-child-") {
        invocation(
            mode,
            deadline,
            Path::new(directory.expect("exact child evidence directory")),
        )
    } else {
        assert!(directory.is_none());
        supervisor(mode, deadline, end)
    }
}
