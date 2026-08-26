/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely useful small utilities.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
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

/// Total wall-clock ALL diagnostic writes may spend waiting for a reader, for the
/// whole life of the process.
///
/// ⚠️ THE BUDGET IS FOR THE PROCESS, NOT FOR ONE `write` CALL, AND THE DIFFERENCE
/// IS NOT SMALL. `write_all` calls `write` repeatedly, and a single error report
/// is several separate `writeln!`s -- `display_error` issues three, and the
/// tracing writer issues one per log line. A budget scoped to one call is
/// therefore multiplied by however many writes remain, so "five seconds" would
/// mean fifteen on the ordinary guest-not-found path and 5s x lines under
/// `--log info`. That is not the ceiling a reader of the constant thinks it is.
///
/// Waiting for a SLOW reader is the feature; waiting for a STOPPED one is a hang
/// on the path that reports why hermit is stopping. This bounds the second
/// without capping the first per-call, by charging every wait to one budget.
const STDERR_WAIT_BUDGET: Duration = Duration::from_secs(5);

/// ⚠️ RESIDUAL, MEASURED AND NOT FIXED HERE: THIS BOUNDS ONE OS PROCESS, AND ONE
/// HERMIT INVOCATION IS SEVERAL. A `static` lives per process, so each hermit
/// process that writes diagnostics carries its OWN accumulator and its own full
/// budget. Counted directly on 2026-08-26 by reading `/proc/*/fd/2` for the pipe
/// inode while a run was in flight:
///
/// ```text
///   distinct processes holding that stderr pipe on fd 2: 3
///     pid 440249  hermit
///     pid 440255  hermit
///     pid 440257  true      (the guest; writes no diagnostics)
/// ```
///
/// Two writing hermit processes, so the ceiling an invocation actually observes is
/// about TWO budgets, not one. Measured end to end at `--log info` against a
/// stopped reader: 10.66s extra over a 0.02s baseline, against a 5s budget. At
/// `--log warn` the extra is 0.01s, because too little is written to fill the pipe
/// at all -- the cost appears only when there is enough output to block.
///
/// So this is a real ceiling and it is NOT the one-budget ceiling the name suggests
/// on its own. It is stated here rather than left to be discovered because an
/// unexplained partial bound reads as a complete one. A true per-invocation ceiling
/// needs a deadline shared across the process tree -- an absolute time computed
/// once and inherited by children rather than a per-process accumulator -- which is
/// filed separately and deliberately not attempted here.
///
/// ⚠️ ALSO OBSERVED, SAME MEASUREMENT, NOT ADDRESSED: a stopped reader turns an
/// otherwise successful run's exit status from 0 into 125 at `--log info`. The
/// diagnostic path failing still moves the exit code, which is the same shape as
/// the defect this machinery exists to bound.
/// Wall-clock already spent waiting on stderr across this process, in
/// milliseconds. Charged against `STDERR_WAIT_BUDGET` by every diagnostic write.
///
/// `Relaxed` is sufficient: this is an approximate accumulator whose only reader
/// is the comparison below, and over-shooting by one poll interval is harmless
/// where the alternative is not returning at all.
static STDERR_WAITED_MS: AtomicU64 = AtomicU64::new(0);

/// Reset the accumulated stderr wait. Test-only: brackets that measure the budget
/// need each case to start from zero, and nothing in a real run wants this.
#[doc(hidden)]
pub fn reset_stderr_wait_budget_for_test() {
    STDERR_WAITED_MS.store(0, Ordering::Relaxed);
}

/// The budget, exposed so a bracket can pin it BY VALUE rather than restating a
/// literal that could drift away from the code it claims to describe.
#[doc(hidden)]
pub fn stderr_wait_budget() -> Duration {
    STDERR_WAIT_BUDGET
}

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
                    // ⚠️ BOUNDED. Waiting for a slow reader is the point; waiting
                    // for a stopped one is a hang on hermit's exit path. Give up
                    // and let the caller drop the line rather than never return.
                    // Charged against the PROCESS budget, not this call's own
                    // elapsed time, so three writeln!s cannot each buy the full
                    // ceiling. See STDERR_WAIT_BUDGET.
                    if STDERR_WAITED_MS.load(Ordering::Relaxed)
                        >= STDERR_WAIT_BUDGET.as_millis() as u64
                    {
                        return Err(err);
                    }
                    let waited = std::time::Instant::now();
                    // Block until the reader makes room. A failed or timed-out
                    // poll falls through to another write attempt rather than
                    // dropping the bytes.
                    let mut pfd = libc::pollfd {
                        fd: libc::STDERR_FILENO,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: one initialised `pollfd`, count 1, timeout in ms.
                    unsafe {
                        libc::poll(&mut pfd, 1, 1000);
                    }
                    STDERR_WAITED_MS.fetch_add(
                        waited.elapsed().as_millis() as u64,
                        Ordering::Relaxed,
                    );
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
