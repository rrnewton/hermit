use std::cell::Cell;

use super::*;

fn text(failure: Failure) -> String {
    String::from_utf8(
        Diagnostic::format(format_args!("{failure}"))
            .bytes()
            .to_vec(),
    )
    .unwrap()
}

#[test]
fn installation_refusal_retains_the_exact_policy_error() {
    let reason = "SUD-only shared-clock execution needs an owned signal-mask policy";
    assert_eq!(
        text(Failure::Installation(io::Error::new(
            io::ErrorKind::Unsupported,
            reason
        ))),
        format!(
            "hermit-liteinst startup failed: stage=installation kind=Unsupported error={reason}\n"
        )
    );
}

#[test]
fn stages_distinguish_bootstrap_transport_formatter_and_missing_log() {
    let cases = [
        (
            Failure::MissingBootstrap,
            "bootstrap error=sealed bootstrap not found",
        ),
        (
            Failure::MissingLog,
            "log-endpoint error=sealed log endpoint missing",
        ),
        (
            Failure::Bootstrap(io::Error::from_raw_os_error(libc::EBADF)),
            "bootstrap kind=Uncategorized errno=9",
        ),
        (
            Failure::Transport(io::Error::from_raw_os_error(libc::EPIPE)),
            "log-transport kind=BrokenPipe errno=32",
        ),
        (
            Failure::Formatter(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid formatter limits",
            )),
            "formatter kind=InvalidInput error=invalid formatter limits",
        ),
        (
            Failure::Record(Arc::new(RecordFailure::Unwinding)),
            "record-status error=host record formatting or publication unwound",
        ),
    ];
    for (failure, expected) in cases {
        assert_eq!(
            text(failure),
            format!("hermit-liteinst startup failed: stage={expected}\n")
        );
    }
}

#[test]
fn actual_codec_errors_distinguish_payload_and_filter_without_secrets() {
    for (payload, stage) in [
        (b"{\"private-secret\":1}".to_vec(), "payload"),
        (
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "tool": "hermit-detcore-liteinst-v1",
                "config_wire_fingerprint": detcore::config_wire_fingerprint(),
                "log_filter": "private-secret=not-a-level"
            }))
            .unwrap(),
            "filter",
        ),
    ] {
        let error = crate::validate_payload(&payload).unwrap_err();
        let failure = Failure::Payload(error.clone());
        let message = text(failure);
        assert_eq!(
            message,
            format!(
                "hermit-liteinst startup failed: stage={stage} error=invalid sealed input detail=[redacted]\n"
            )
        );
        assert!(!message.contains("private-secret"));
        assert!(
            matches!(Failure::Payload(error.clone()), Failure::Payload(retained) if retained == error)
        );
    }
}

#[test]
fn sealed_schema_refusals_keep_safe_exact_reasons() {
    for reason in [
        "unsupported bootstrap payload version",
        "bootstrap tool mismatch",
        "bootstrap config fingerprint mismatch",
    ] {
        assert_eq!(
            text(Failure::Payload(reason.to_owned())),
            format!("hermit-liteinst startup failed: stage=payload error={reason}\n")
        );
    }
}

#[test]
fn opaque_decode_and_rpc_details_are_not_bootstrap_dumps() {
    for kind in [io::ErrorKind::InvalidData, io::ErrorKind::Other] {
        let message = text(Failure::Installation(io::Error::new(
            kind,
            "private-secret",
        )));
        assert!(!message.contains("private-secret"));
        assert!(message.contains("redacted: may contain bootstrap data"));
    }
}

#[test]
fn selector_and_success_states_do_not_emit_success_markers() {
    assert!(!selected(None).unwrap());
    assert!(selected(Some(OsStr::new(crate::BOOTSTRAP_SELECTOR))).unwrap());
    assert!(matches!(
        selected(Some(OsStr::new("private-secret"))),
        Err(Failure::Selection)
    ));
    assert_eq!(
        text(Failure::Selection),
        "hermit-liteinst startup failed: stage=selection error=unrecognized selector\n"
    );
    for status in [0, 1] {
        assert_eq!(
            finish(Ok(status), |_| panic!(
                "successful or unselected constructor wrote stderr"
            )),
            (status, None)
        );
    }
}

#[test]
fn format_once_truncates_explicitly_at_a_utf8_boundary() {
    struct Message<'counter>(&'counter Cell<usize>);
    impl fmt::Display for Message<'_> {
        fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.set(self.0.get() + 1);
            for _ in 0..CAPACITY {
                output.write_str("é")?;
            }
            Ok(())
        }
    }
    let calls = Cell::new(0);
    let diagnostic = Diagnostic::format(format_args!("{}", Message(&calls)));
    assert_eq!(calls.get(), 1);
    assert!(diagnostic.bytes().len() <= CAPACITY);
    assert!(diagnostic.bytes().ends_with(TRUNCATED));
    let expected = format!(
        "{} [truncated]\n",
        "é".repeat((CAPACITY - FORMAT_FAILED.len()) / 2)
    );
    assert_eq!(diagnostic.bytes(), expected.as_bytes());
}

#[test]
fn exact_boundary_and_control_characters_preserve_one_line() {
    let full = "x".repeat(CAPACITY - FORMAT_FAILED.len());
    assert_eq!(
        Diagnostic::format(format_args!("{full}")).bytes(),
        format!("{full}\n").as_bytes()
    );
    assert_eq!(
        Diagnostic::format(format_args!("one\ntwo\r\t\0")).bytes(),
        b"one\\ntwo\\r\\t?\n"
    );
}

#[test]
fn formatter_error_is_not_reported_as_a_complete_message() {
    struct Bad;
    impl fmt::Display for Bad {
        fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
            output.write_str("prefix")?;
            Err(fmt::Error)
        }
    }
    assert_eq!(
        Diagnostic::format(format_args!("{Bad}")).bytes(),
        b"prefix [formatting failed]\n"
    );
}

#[test]
fn interrupted_and_short_writes_emit_exact_bytes_without_reformatting() {
    let mut calls = 0;
    let mut received = Vec::new();
    let report = deliver(b"abcdef", |bytes| {
        calls += 1;
        if calls == 1 {
            return -(libc::EINTR as i64);
        }
        let used = bytes.len().min(2);
        received.extend_from_slice(&bytes[..used]);
        used as i64
    });
    assert_eq!(received, b"abcdef");
    assert_eq!(calls, 4);
    assert_eq!(
        report,
        Delivery {
            written: 6,
            failure: None
        }
    );
}

#[test]
fn failed_writes_retain_only_the_acknowledged_prefix_and_errno() {
    for (raw, failure) in [
        (-(libc::EPIPE as i64), WriteFailure::Errno(libc::EPIPE)),
        (-(libc::EBADF as i64), WriteFailure::Errno(libc::EBADF)),
        (-(libc::EAGAIN as i64), WriteFailure::Errno(libc::EAGAIN)),
        (0, WriteFailure::Zero),
        (9999, WriteFailure::InvalidResult(9999)),
        (i64::MIN, WriteFailure::InvalidResult(i64::MIN)),
    ] {
        let mut calls = 0;
        let report = deliver(b"abcdef", |bytes| {
            calls += 1;
            if calls == 1 {
                return 2;
            }
            assert_eq!(bytes, b"cdef");
            raw
        });
        assert_eq!(calls, 2);
        assert_eq!(
            report,
            Delivery {
                written: 2,
                failure: Some(failure)
            }
        );
    }
}

#[test]
fn repeated_interrupts_and_short_writes_have_a_fixed_attempt_budget() {
    let mut calls = 0;
    let report = deliver(b"x", |_| {
        calls += 1;
        -(libc::EINTR as i64)
    });
    assert_eq!(calls, MAX_WRITES);
    assert_eq!(
        report,
        Delivery {
            written: 0,
            failure: Some(WriteFailure::AttemptLimit)
        }
    );
    assert_eq!(
        deliver(&[0; MAX_WRITES + 1], |_| 1),
        Delivery {
            written: MAX_WRITES,
            failure: Some(WriteFailure::AttemptLimit)
        }
    );
    assert_eq!(
        deliver(&[0; MAX_WRITES], |_| 1),
        Delivery {
            written: MAX_WRITES,
            failure: None
        }
    );
}

#[test]
fn diagnostic_write_failure_does_not_replace_the_startup_exit() {
    let reason = "SUD-only shared-clock execution needs an owned signal-mask policy";
    let mut seen = Vec::new();
    let (status, delivery) = finish(
        Err(Failure::Installation(io::Error::new(
            io::ErrorKind::Unsupported,
            reason,
        ))),
        |bytes| {
            seen.extend_from_slice(bytes);
            -(libc::EBADF as i64)
        },
    );
    assert_eq!(status, 127);
    assert_eq!(
        delivery,
        Some(Delivery {
            written: 0,
            failure: Some(WriteFailure::Errno(libc::EBADF))
        })
    );
    assert!(std::str::from_utf8(&seen).unwrap().contains(reason));
}

#[test]
fn subscriber_failure_is_distinct_in_an_isolated_host_process() {
    const CHILD: &str = "HERMIT_STARTUP_SUBSCRIBER_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry().try_init().unwrap();
        let failure = Failure::Subscriber(tracing_subscriber::registry().try_init().unwrap_err());
        let original = match &failure {
            Failure::Subscriber(error) => error.to_string(),
            _ => unreachable!(),
        };
        assert_eq!(
            text(failure),
            format!("hermit-liteinst startup failed: stage=subscriber error={original}\n")
        );
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "startup::tests::subscriber_failure_is_distinct_in_an_isolated_host_process",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn kernel_mask() -> u64 {
    let mut mask = 0u64;
    let result = unsafe {
        reverie_preload::trap::raw_syscall6(
            libc::SYS_rt_sigprocmask,
            [0, 0, (&raw mut mask) as u64, 8, 0, 0],
        )
    };
    assert_eq!(result, 0);
    mask
}

#[test]
fn owned_setup_refusal_diagnostics_preserve_stage_and_cause() {
    for reason in [
        "SUD-only does not cover instruction or vDSO subscriptions",
        "public owned execution requires only getpid/read syscalls",
    ] {
        let mut output = Vec::new();
        let (status, delivery) = finish_terminal(
            Err(Failure::Installation(io::Error::new(
                io::ErrorKind::Unsupported,
                reason,
            ))),
            || 0,
            |bytes| {
                output.extend_from_slice(bytes);
                bytes.len() as i64
            },
        );
        assert_eq!(status, 127);
        assert_eq!(output, format!("hermit-liteinst startup failed: stage=installation kind=Unsupported error={reason}\n").as_bytes());
        assert_eq!(
            delivery,
            Some(Delivery {
                written: output.len(),
                failure: None
            })
        );
    }
}

#[test]
fn terminal_success_and_unselected_leave_signal_state_and_output_untouched() {
    for status in [0, 1] {
        assert_eq!(
            finish_terminal(
                Ok(status),
                || panic!("nonfatal startup changed signal state"),
                |_| panic!("nonfatal startup wrote a diagnostic"),
            ),
            (status, None)
        );
    }
}

#[test]
fn terminal_mask_failure_skips_output_and_preserves_selected_exit() {
    for result in [-(libc::EPERM as i64), -(libc::EINTR as i64), 1, i64::MIN] {
        assert_eq!(
            finish_terminal(
                Err(Failure::MissingBootstrap),
                || result,
                |_| panic!("write attempted without a successful terminal mask"),
            ),
            (
                127,
                Some(Delivery {
                    written: 0,
                    failure: Some(WriteFailure::SignalMask(result)),
                })
            )
        );
    }
}

#[test]
fn terminal_mask_precedes_output_and_write_errors_preserve_selected_exit() {
    for errno in [libc::EPIPE, libc::EBADF] {
        let masked = std::cell::Cell::new(false);
        let (status, delivery) = finish_terminal(
            Err(Failure::MissingBootstrap),
            || {
                assert!(!masked.replace(true));
                0
            },
            |bytes| {
                assert!(masked.get());
                assert_eq!(bytes, b"hermit-liteinst startup failed: stage=bootstrap error=sealed bootstrap not found\n");
                -(errno as i64)
            },
        );
        assert_eq!(status, 127);
        assert_eq!(
            delivery,
            Some(Delivery {
                written: 0,
                failure: Some(WriteFailure::Errno(errno))
            })
        );
    }
}

#[test]
fn terminal_output_preserves_exit_on_a_closed_pipe_with_default_sigpipe() {
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    const CHILD: &str = "HERMIT_TERMINAL_STDERR_TEST_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut()) },
            0
        );
        let pipe_bit = 1u64 << (libc::SIGPIPE - 1);
        assert_eq!(
            unsafe {
                reverie_preload::trap::raw_syscall6(
                    libc::SYS_rt_sigprocmask,
                    [
                        libc::SIG_UNBLOCK as u64,
                        (&raw const pipe_bit) as u64,
                        0,
                        8,
                        0,
                        0,
                    ],
                )
            },
            0
        );
        let before = kernel_mask();
        let result = match mode.to_str().unwrap() {
            "failure" => Err(Failure::MissingBootstrap),
            "unselected" => Ok(0),
            "success" => Ok(1),
            _ => panic!("unexpected child mode"),
        };
        let status = unsafe { finish_initializer(result) };
        assert_eq!(
            kernel_mask(),
            if status == 127 {
                before | pipe_bit
            } else {
                before
            }
        );
        let mut after: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut after) },
            0
        );
        assert_eq!(after.sa_sigaction, libc::SIG_DFL);
        unsafe {
            reverie_preload::trap::raw_syscall6(
                libc::SYS_exit_group,
                [status as u64, 0, 0, 0, 0, 0],
            )
        };
        panic!("terminal exit returned");
    }

    for (mode, expected) in [("failure", 127), ("unselected", 0), ("success", 1)] {
        for closed in [true, false] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "startup::tests::terminal_output_preserves_exit_on_a_closed_pipe_with_default_sigpipe"])
                .env(CHILD, mode);
            if closed {
                let mut descriptors = [0; 2];
                assert_eq!(
                    unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
                    0
                );
                let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
                let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
                drop(reader);
                command.stderr(Stdio::from(writer));
            }
            let output = command.output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(expected),
                "mode={mode} closed={closed} signal={:?} output={output:?}",
                output.status.signal()
            );
            if !closed && mode == "failure" {
                assert_eq!(output.stderr, b"hermit-liteinst startup failed: stage=bootstrap error=sealed bootstrap not found\n");
            } else {
                assert!(output.stderr.is_empty(), "{output:?}");
            }
        }
    }
}
