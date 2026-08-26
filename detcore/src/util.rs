/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely useful small utilities.

use std::sync::OnceLock;
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

/// Total wall-clock ALL diagnostic writes may spend waiting for a reader, across
/// EVERY hermit process in one invocation.
///
/// ⚠️ THE BUDGET IS FOR THE INVOCATION, NOT FOR ONE `write` CALL AND NOT FOR ONE
/// PROCESS. Both narrower readings have already been shipped and both understated
/// the real ceiling by a whole factor:
///
/// * Per `write` CALL. `write_all` calls `write` repeatedly and a single error
///   report is several `writeln!`s -- `display_error` issues three, the tracing
///   writer one per log line -- so the ceiling was multiplied by however many
///   writes remained. Measured at 20.4s against a 5s constant.
/// * Per PROCESS. A `static` lives per process, so each hermit process carried
///   its own full budget. Measured at 10.66s against the same 5s constant.
///
/// ⚠️ WHERE THE NUMBER COMES FROM: IT IS DERIVED FROM THE RUNG OUTSIDE IT, NOT
/// CHOSEN. `docs/TIMEOUT_LADDER.md` requires each rung to be strictly smaller
/// than the rung enclosing it, because an inner bound at or above its outer bound
/// can never fire and is dead configuration that reads as protection.
///
/// This wait happens on hermit's way OUT, so the rung enclosing it is hermit's
/// own unwind grace, `RUN_TIMEOUT_UNWIND_GRACE` in `hermit-cli/src/lib.rs`,
/// which is 10s. The arithmetic against the previous per-process bound:
///
/// ```text
///   hermit processes that write diagnostics   2   (measured 2026-08-26; see below)
///   per-process budget                        5s
///   invocation ceiling                     2 x 5s = 10s
///   RUN_TIMEOUT_UNWIND_GRACE                       10s
///                                                  ^ EQUAL, not smaller
/// ```
///
/// Equal is the failure. And the end-to-end measurement was 10.66s, so it did not
/// merely tie the grace, it EXCEEDED it. Spending the whole grace on diagnostics
/// makes the unwind fallback fire and print `HERMIT_RUN_TIMEOUT_FALLBACK`, which
/// `docs/TIMEOUT_LADDER.md` defines as "the unwind itself did not finish -- this
/// is a hermit defect, not a slow guest". So an over-large diagnostic budget does
/// not just waste time; it MANUFACTURES a false hermit-defect signal and destroys
/// the meaning of that marker, the same way the ladder describes for `kvm`.
///
/// So the invocation-wide ceiling is derived as HALF the enclosing grace, leaving
/// the other half for the unwind that grace exists to cover:
///
/// ```text
///   RUN_TIMEOUT_UNWIND_GRACE / 2 = 5s   invocation-wide ceiling
///   share of the outermost hermit       2.5s   (see stderr_wait_budget)
///   sum over a tree of ANY depth      < 5s
/// ```
///
/// `hermit-cli/tests/cli.rs` pins that relationship against the real
/// `RUN_TIMEOUT_UNWIND_GRACE` so the two cannot drift apart silently.
const STDERR_TREE_WAIT_BUDGET: Duration = Duration::from_millis(5_000);

/// This process's share of [`STDERR_TREE_WAIT_BUDGET`].
///
/// ⚠️ THE SHARE HALVES AT EACH NESTING LEVEL SO THE TOTAL IS BOUNDED WITHOUT ANY
/// CHANNEL BETWEEN THE PROCESSES. A hermit nested `d` levels inside another
/// hermit takes `STDERR_TREE_WAIT_BUDGET / 2^(d+1)`, so the outermost gets 2.5s,
/// a child 1.25s, a grandchild 0.625s, and the sum over a chain of ANY depth
/// stays strictly under the 5s invocation ceiling. Nothing has to be shared,
/// inherited or agreed: each process computes its own share and the arithmetic
/// does the rest.
///
/// ⚠️ WHY NOT SIMPLY DIVIDE BY THE PROCESS COUNT. Measured 2026-08-26, hermit
/// runs as exactly two processes in a parent/child chain, and that did NOT change
/// with the guest's own process count:
///
/// ```text
///   guest with 1 process    max concurrent hermit processes = 2   (373462 -> 373472)
///   guest with 4 processes  max concurrent hermit processes = 2   (377374 -> 377384)
/// ```
///
/// Two is therefore the shape today, but dividing by a hard-coded 2 would silently
/// become wrong the day a third appears, and it would be wrong in the direction
/// that overshoots the grace -- which is exactly how the two earlier versions of
/// this bound failed. Halving per level cannot overshoot however deep the tree
/// turns out to be, so it does not need the count to stay 2.
///
/// The nesting depth comes from walking `/proc/<pid>/stat` upward while the parent
/// is also `hermit`. That is a bounded walk over `/proc` with no new inter-process
/// channel. ⚠️ IT DELIBERATELY DOES NOT USE THE ENVIRONMENT: a value set on
/// hermit's environment reaches the GUEST by default (`BaseEnv::Host` does not
/// clear it, measured), and a per-run-varying value visible to the guest, in a
/// determinism tool, on a surface the argv/env hashing covers, would be a worse
/// defect than the one being fixed.
fn stderr_process_wait_budget() -> Duration {
    static SHARE_MS: OnceLock<u64> = OnceLock::new();
    let share_ms = *SHARE_MS.get_or_init(|| {
        let depth = hermit_nesting_depth().min(16);
        // `>> (depth + 1)` is the halving above: depth 0 -> /2, depth 1 -> /4.
        // Saturates at 1ms rather than 0 so a deeply nested process still makes
        // one attempt rather than dropping its first diagnostic unconditionally.
        ((STDERR_TREE_WAIT_BUDGET.as_millis() as u64) >> (depth + 1)).max(1)
    });
    Duration::from_millis(share_ms)
}

/// How many `hermit` processes enclose this one, by walking `/proc` upward.
///
/// Returns 0 for the outermost hermit. Any unreadable or malformed `/proc` entry
/// stops the walk and yields the depth found so far, which under-counts rather
/// than over-counts -- an under-count gives this process a LARGER share, so the
/// failure direction is a slightly looser bound rather than a diagnostic dropped
/// on its first attempt. The loop is capped independently of that.
fn hermit_nesting_depth() -> u32 {
    let mut depth = 0;
    let mut pid = match proc_stat_ppid(std::process::id()) {
        Some(ppid) => ppid,
        None => return 0,
    };
    // Capped so a malformed or cyclic /proc cannot spin on hermit's exit path.
    for _ in 0..16 {
        if pid <= 1 || !proc_comm_is_hermit(pid) {
            break;
        }
        depth += 1;
        pid = match proc_stat_ppid(pid) {
            Some(ppid) => ppid,
            None => break,
        };
    }
    depth
}

fn proc_comm_is_hermit(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim_end() == "hermit")
        .unwrap_or(false)
}

/// The parent pid from `/proc/<pid>/stat`, field 4.
///
/// ⚠️ FIELD 2 IS THE COMM AND IT CAN CONTAIN SPACES AND PARENTHESES, so the fields
/// cannot simply be split on whitespace. Everything after the LAST `)` is parsed
/// instead, which is the standard way to read this file and the reason a naive
/// split misreads any process whose name contains a space.
fn proc_stat_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Wall-clock already spent waiting on stderr in THIS process, in milliseconds.
/// Charged against [`stderr_process_wait_budget`] by every diagnostic write.
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

/// This process's share, exposed so a bracket can pin it BY VALUE rather than
/// restating a literal that could drift away from the code it claims to describe.
#[doc(hidden)]
pub fn stderr_wait_budget() -> Duration {
    stderr_process_wait_budget()
}

/// The invocation-wide ceiling, exposed so a bracket can pin it against
/// `RUN_TIMEOUT_UNWIND_GRACE` and fail closed if the two ever cross.
#[doc(hidden)]
pub fn stderr_tree_wait_budget() -> Duration {
    STDERR_TREE_WAIT_BUDGET
}

/// This process's nesting depth, exposed so a bracket can assert the sum over a
/// real tree rather than over an assumed one.
#[doc(hidden)]
pub fn stderr_wait_nesting_depth() -> u32 {
    hermit_nesting_depth()
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
                    // Charged against this process's SHARE of the invocation-wide
                    // ceiling, not against this call's own elapsed time and not
                    // against a whole budget per process -- so neither three
                    // writeln!s nor a second hermit can buy the ceiling again.
                    // See STDERR_TREE_WAIT_BUDGET.
                    if STDERR_WAITED_MS.load(Ordering::Relaxed)
                        >= stderr_process_wait_budget().as_millis() as u64
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
