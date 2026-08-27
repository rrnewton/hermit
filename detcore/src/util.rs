/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely useful small utilities.

use std::time::Duration;

use crate::types::NANOS_PER_RCB;

#[allow(dead_code)]
/// A simple debugging helper function that makes it easy to printf-debug through
/// layers of stdout/stderr caputure, such as when running under buck test/tpx.
pub fn punch_out_print(msg: &str) {
    use std::io::Write;
    // TODO: if we want this to be more performant, we can have a lazy static
    // global file handle for this. This, however, keeps it simple for occasional usage.œ
    if let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        writeln!(tty, "{}", msg).unwrap();
    } else {
        // If devtty doesn't exist, we just use stderr.
        eprintln!("{}", msg);
    }
}
/// A helper function to convert a number of Retired Conditional Branches (RCBS) into
/// a `std::time::Duration` via the `NANOS_PER_RCB` defined in ` types.rs`.
pub fn rcbs_to_duration(rcbs: u64) -> Duration {
    Duration::from_nanos((rcbs as f64 * NANOS_PER_RCB) as u64)
}

/// A little better than the builtin string truncation in format strings, because it includes ellipses.
// TODO: There should be some advanced solution for printing potentially huge things that
// doesn't actually render them all...
pub fn truncated(width: usize, mut s: String) -> String {
    if s.len() > width {
        if width >= 3 {
            s.truncate(width - 3);
            s.push_str("...");
            s
        } else {
            s.truncate(width);
            s
        }
    } else {
        s
    }
}

/// A `Write` for the supervisor's own diagnostics on fd 2 that survives a
/// NONBLOCKING description.
///
/// ⚠️ THE GUEST CAN SET `O_NONBLOCK` ON HERMIT'S STDERR AND SILENTLY CUT THE
/// SUPERVISOR'S ERROR CHANNEL. fd 2 is an INHERITED open file description shared
/// with the guest, so `fcntl(2, F_SETFL, O_NONBLOCK)` in the guest changes the
/// behaviour of hermit's OWN later writes. Measured 2026-08-26 on a 4096-byte
/// pipe under back-pressure, `hermit --log info run -- <guest>`:
///
/// ```text
///   control guest (touches nothing)   rc=0    138 lines, ends on the run summary
///   guest sets O_NONBLOCK on fd 2     rc=101  134 lines, ends mid-log
/// ```
///
/// The summary was emitted by `eprint!`, which calls `write_all`; `write_all`
/// does NOT retry `EAGAIN`, so it returns an error and the print macro PANICS.
/// The panic message then went to the same full pipe and was lost as well, so
/// the delivered output contained no panic text, no error, and no marker of any
/// kind -- it simply stopped on a plausible-looking line. Exit 101 was the only
/// surviving evidence, and a caller that reads output rather than status sees a
/// short report, not a truncated one.
///
/// ⚠️ WAIT, DO NOT SPIN, AND DO NOT CLEAR THE FLAG. Clearing `O_NONBLOCK` would
/// change what the guest observes on a descriptor it legitimately shares; this
/// leaves the flag exactly as the guest set it and simply waits for the pipe to
/// drain, which is what a blocking write would have done. A bare retry loop
/// would busy-spin against a full pipe, so it blocks in `poll(POLLOUT)`.
pub struct RetryingStderr;

/// Total wall-clock ALL diagnostic writes in ONE PROCESS may spend waiting for a
/// reader that is not draining. Hermit forks, so read the fork note below before
/// treating this as the whole exit path's budget.
///
/// ⚠️ WITHOUT A CEILING THIS LOOP NEVER ENDS, AND IT RUNS WHILE HERMIT IS ALREADY
/// FAILING. `EAGAIN` -> `poll(POLLOUT, 1s)` -> retry is correct for a reader that
/// is SLOW and unbounded for a reader that is STOPPED. Measured 2026-08-26 on a
/// 4096-byte pipe filled to 3900 with `O_NONBLOCK` set, where nothing ever reads:
///
/// ```text
///   before RetryingStderr (eprintln!)   EXITED rc=101 immediately
///   RetryingStderr, no ceiling          STILL RUNNING after 25s, no exit
/// ```
///
/// The first is wrong loudly; the second does not return at all, on the path that
/// reports why hermit is stopping. A supervisor that hangs while reporting an
/// error is harder to diagnose than one that dies reporting it badly.
///
/// ⚠️ THIS IS A TOTAL FOR THE WHOLE PROCESS, NOT A BUDGET PER `write()` CALL, AND
/// THE DIFFERENCE IS THE ENTIRE GUARANTEE. An earlier version started the clock
/// inside `fn write`, so every call got the full allowance. Nothing writes to
/// stderr exactly once on the exit path:
///
/// ```text
///   hermit-cli/src/bin/hermit/main.rs      the failure class, the head of the
///                                          error chain, then ONE PER CAUSE in
///                                          `for cause in chain` -- 2 + chain length
///   hermit-cli/src/bin/hermit/tracing.rs   .with_writer(|| RetryingStderr) is the
///                                          writer for the WHOLE subscriber: one
///                                          write per log event
///   detcore/src/tool_global.rs             a multi-part `write!`, which `write_fmt`
///                                          splits into several `write` calls
/// ```
///
/// With a per-call budget the process-level cost was N times this number, N is
/// bounded by the error chain rather than by anything here, and every caller's
/// `let _ =` swallowed the overrun silently. A `--log info` run measured at 138
/// lines would have been minutes, not seconds.
///
/// ⚠️ THE VALUE IS DERIVED FROM THE BOUND THAT ENCLOSES IT, NOT CHOSEN. Diagnostics
/// on the exit path run inside `RUN_TIMEOUT_UNWIND_GRACE` (10s,
/// `hermit-cli/src/lib.rs`): the window between a `--timeout` expiring and the
/// SIGALRM fallback calling `_exit`. Overrun there does not merely delay the
/// report, it loses it -- the fallback fires mid-sentence and additionally emits
/// `HERMIT_RUN_TIMEOUT_FALLBACK`, which is supposed to mean the teardown wedged.
/// So the arithmetic is:
///
/// ⚠️ AND IT APPLIES TWICE, BECAUSE HERMIT FORKS. `Container::run` forks the
/// container init, so the init gets its own COPY of the process-wide clock below
/// and spends its own full deadline. Both processes write diagnostics to the same
/// fd 2 on the way out. "Process total" is therefore NOT "exit total", and a
/// derivation that forgets the fork is out by exactly a factor of two. Measured
/// 2026-08-26 against a stopped reader on a real guest, which is the case where
/// both processes report:
///
/// ```text
///   deadline 5000ms   ->  10.03s observed   two processes, one deadline each
///   deadline 2500ms   ->   5.02s observed   the same two, halved
///   deadline 2500ms   ->   2.52s observed   missing guest: only ONE process reports
/// ```
///
/// So the arithmetic is:
///
/// ```text
///   RUN_TIMEOUT_UNWIND_GRACE                    10s     the enclosing bound
///   diagnostics may have half of it              5s     leaving 5s for the unwind
///   processes that write diagnostics              2     hermit + the forked init
///   this deadline, per process                2500ms    = 5s / 2
/// ```
///
/// Half is a split, not a measurement, and is stated as one; the factor of two is
/// a fact about the process tree and is measured above. What is ALSO measured is
/// that the previous arrangement could not fit: at 5s per CALL, the same real-guest
/// fixture took 15.04s -- three blocked writes -- against a 10s grace, so the inner
/// bound exceeded its outer bound before any unwind work happened at all.
/// `hermit-cli` asserts this relationship in a test, so raising either side without
/// the other fails by name.
///
/// On expiry the write returns `WouldBlock`, `writeln!` gives up, the caller's
/// `let _ =` drops that line, and hermit exits — the pre-existing contract for a
/// diagnostic that cannot be delivered.
pub const STDERR_DIAGNOSTIC_DEADLINE: std::time::Duration = std::time::Duration::from_millis(2500);

/// When this process first found stderr unwritable, shared by every
/// `RetryingStderr`, which is what makes the deadline above a process total.
/// Set on the first `EAGAIN` and never reset: a run that recovers and later
/// blocks again has still spent that earlier time on its exit path.
static STDERR_BLOCKED_SINCE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

impl std::io::Write for RetryingStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // SAFETY: `write(2)` on fd 2 reads `buf.len()` bytes from `buf`,
            // which is valid for that length, and writes no memory.
            let n = unsafe { libc::write(libc::STDERR_FILENO, buf.as_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {
                    // ⚠️ BOUNDED, AND THE CLOCK IS PROCESS-WIDE. Waiting for a slow
                    // reader is the point; waiting for a stopped one is a hang on
                    // hermit's exit path. Starting the clock here rather than at the
                    // top of `write` is what makes the deadline a total: the first
                    // blocked write sets it and every later one inherits it, so N
                    // writes cost the deadline once instead of N times.
                    let blocked_since = *STDERR_BLOCKED_SINCE.get_or_init(std::time::Instant::now);
                    let spent = blocked_since.elapsed();
                    let Some(remaining) = STDERR_DIAGNOSTIC_DEADLINE.checked_sub(spent) else {
                        return Err(err);
                    };
                    // Block until the reader makes room. A failed or timed-out
                    // poll falls through to another write attempt rather than
                    // dropping the bytes.
                    let mut pfd = libc::pollfd {
                        fd: libc::STDERR_FILENO,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // ⚠️ CLAMPED TO WHAT IS LEFT, NOT A FLAT SECOND. The check above
                    // happens BEFORE the poll, so a flat 1000ms let a call that
                    // started at 4.9s sleep a further second and overshoot to ~6s --
                    // the deadline documented one number and delivered another.
                    // Clamping makes the elapsed total the stated total.
                    let timeout_ms = i32::try_from(remaining.as_millis())
                        .unwrap_or(i32::MAX)
                        .min(1000);
                    // SAFETY: one initialised `pollfd`, count 1, timeout in ms.
                    unsafe {
                        libc::poll(&mut pfd, 1, timeout_ms);
                    }
                    continue;
                }
                _ => return Err(err),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
