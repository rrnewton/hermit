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
///
/// ⚠️ RESIDUAL, CARRIED FORWARD FROM THE PER-PROCESS VERSION AND STILL NOT FIXED:
/// a stopped reader turns an otherwise successful run's exit status from 0 into
/// 125 at `--log info`. The diagnostic path failing still moves the exit code,
/// which is the same shape as the defect this machinery exists to bound. Bounding
/// the WAIT does not address it -- giving up on the write is exactly when the
/// status moves -- so it is restated here rather than dropped along with the
/// residual this change did close.
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
    let share_ms = *SHARE_MS
        .get_or_init(|| stderr_share_for_depth(hermit_nesting_depth()).as_millis() as u64);
    Duration::from_millis(share_ms)
}

/// The share arithmetic on its own, so it can be exercised at depths this process
/// is not actually running at.
///
/// ⚠️ THE TEST MUST CALL THIS RATHER THAN RESTATE IT. An earlier version of the
/// bracket recomputed the same shift inline and therefore passed against a build
/// with the halving removed entirely -- it was checking a copy of the arithmetic
/// against itself. Caught by mutation, not by reading it.
fn stderr_share_for_depth(depth: u32) -> Duration {
    // `>> (depth + 1)` is the halving: depth 0 -> /2, depth 1 -> /4. Saturates at
    // 1ms rather than 0 so a deeply nested process still makes one attempt rather
    // than dropping its first diagnostic unconditionally.
    let ms = ((STDERR_TREE_WAIT_BUDGET.as_millis() as u64) >> (depth.min(16) + 1)).max(1);
    Duration::from_millis(ms)
}

/// How many `hermit` processes enclose this one, by walking `/proc` upward.
///
/// Returns 0 for the outermost hermit. Any unreadable or malformed `/proc` entry
/// stops the walk and yields the depth found so far, which under-counts rather
/// than over-counts -- an under-count gives this process a LARGER share, so the
/// failure direction is a slightly looser bound rather than a diagnostic dropped
/// on its first attempt. The loop is capped independently of that.
fn hermit_nesting_depth() -> u32 {
    hermit_nesting_depth_of(std::process::id(), proc_stat_ppid, proc_comm_is_hermit)
}

/// The walk with its inputs injected, so the cases `/proc` cannot be put into
/// during a unit test can still be exercised.
fn hermit_nesting_depth_of(
    me: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    comm_is_hermit: impl Fn(u32) -> bool,
) -> u32 {
    // ⚠️ THE NESTED HERMIT IS PID 1 IN ITS OWN PID NAMESPACE, SO THE WALK CANNOT
    // SEE ITS PARENT AND MUST NOT CONCLUDE IT HAS NONE. Measured 2026-08-26 from
    // outside the container, `NSpid` for the two hermit processes of one run:
    //
    // ```text
    //   host_pid=1455593  NSpid: 1455593      host_ppid=1455493 (bash)
    //   host_pid=1455601  NSpid: 1455601 1    host_ppid=1455593 (hermit)
    // ```
    //
    // The second column is the point: the inner hermit is pid 1 inside the pid
    // namespace hermit builds for the container, so from in there `/proc/1/stat`
    // is ITSELF and reports parent 0. Reading that as "no hermit above me" would
    // give the inner process the OUTERMOST share, making the total 2.5s + 2.5s =
    // 5s -- exactly the ceiling rather than under it, which is the same
    // equal-not-smaller defect this whole change exists to remove, reintroduced
    // one level down.
    //
    // Being pid 1 without being the machine's init therefore counts as one level
    // of nesting on its own. If hermit is ever genuinely run as pid 1 this
    // under-shares rather than over-shares, which keeps the total bounded.
    if me == 1 {
        return 1;
    }
    let mut depth = 0;
    let mut pid = match parent_of(me) {
        Some(ppid) => ppid,
        None => return 0,
    };
    // Capped so a malformed or cyclic /proc cannot spin on hermit's exit path.
    for _ in 0..16 {
        if pid <= 1 || !comm_is_hermit(pid) {
            break;
        }
        depth += 1;
        pid = match parent_of(pid) {
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
                    let share_ms = stderr_process_wait_budget().as_millis() as u64;
                    let spent_ms = STDERR_WAITED_MS.load(Ordering::Relaxed);
                    let Some(remaining_ms) = share_ms.checked_sub(spent_ms).filter(|r| *r > 0)
                    else {
                        return Err(err);
                    };
                    let waited = std::time::Instant::now();
                    // Block until the reader makes room. A failed or timed-out
                    // poll falls through to another write attempt rather than
                    // dropping the bytes.
                    let mut pfd = libc::pollfd {
                        fd: libc::STDERR_FILENO,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // ⚠️ THE POLL IS CLAMPED TO WHAT IS LEFT, AND WITHOUT THAT THE
                    // BOUND DOES NOT HOLD. Checking the budget only BEFORE a poll
                    // that can block for a fixed 1000ms lets each process overshoot
                    // its share by almost a full second, and every process in the
                    // tree overshoots independently. Measured with the fixed 1000ms
                    // poll and a 5s invocation ceiling: 6.37s. That is the same
                    // defect as charging per call -- a bound tested at the wrong
                    // granularity to the thing it bounds -- one level further down.
                    let timeout_ms = remaining_ms.min(1000) as libc::c_int;
                    // SAFETY: one initialised `pollfd`, count 1, timeout in ms.
                    unsafe {
                        libc::poll(&mut pfd, 1, timeout_ms);
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

#[cfg(test)]
mod stderr_wait_tests {
    use super::*;

    /// Drive the real writer against a real full pipe and measure the real wait.
    ///
    /// ⚠️ THIS IS THE ONLY TEST THAT MEASURES THE BOUND, AND THE END-TO-END ONES IN
    /// `hermit-cli/tests/cli.rs` DO NOT, WHICH IS MEASURED RATHER THAN ASSUMED. Those
    /// compare a subprocess's total elapsed time against a baseline, and that
    /// difference is dominated by something this mechanism does not control.
    /// Instrumented on 2026-08-26 to print each process's accounting at give-up,
    /// three runs of the same fixture:
    ///
    /// ```text
    ///   elapsed=5.51s   spent_ms=2500   (one process, depth 0)
    ///   elapsed=10.88s  spent_ms=2500   (one process, depth 0)
    ///   elapsed=5.41s   spent_ms=2500   (one process, depth 0)
    /// ```
    ///
    /// The wait is EXACTLY the share every time; the elapsed time varies by 5.5s
    /// with the wait held constant. So an assertion on elapsed-minus-baseline is
    /// not an assertion about this budget, and the 10.88s run would read as "two
    /// budgets were spent" when one was. Measuring the writer directly, in this
    /// process, is what removes that confound.
    #[test]
    fn the_wait_stops_at_this_processes_share() {
        use std::io::Write;

        // A pipe nobody reads, with the smallest capacity the kernel will take, so
        // the writer blocks rather than fitting the payload.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        let (read_fd, write_fd) = (fds[0], fds[1]);
        const F_SETPIPE_SZ: i32 = 1031;
        unsafe { libc::fcntl(write_fd, F_SETPIPE_SZ, 4096) };
        let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
        unsafe { libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };

        // `RetryingStderr` writes fd 2 by construction -- that is the descriptor it
        // exists to protect -- so point fd 2 at the full pipe for the duration.
        let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved_stderr >= 0, "dup stderr");
        assert!(
            unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) } >= 0,
            "dup2 the full pipe onto fd 2"
        );

        reset_stderr_wait_budget_for_test();
        let share = stderr_process_wait_budget();
        let payload = vec![b'x'; 64 * 1024];

        let started = std::time::Instant::now();
        // Keep writing until the writer refuses. Each call charges the same
        // accumulator, so the total is the share however many calls it takes.
        let mut refused = false;
        for _ in 0..64 {
            if RetryingStderr.write(&payload).is_err() {
                refused = true;
                break;
            }
        }
        let elapsed = started.elapsed();

        // Restore fd 2 before any assertion can print.
        unsafe {
            libc::dup2(saved_stderr, libc::STDERR_FILENO);
            libc::close(saved_stderr);
            libc::close(read_fd);
            libc::close(write_fd);
        }

        assert!(
            refused,
            "the writer never refused against a pipe nobody reads, so the bound was \
             not exercised at all"
        );
        // ⚠️ UPPER BOUND WITH A REAL TOLERANCE, NOT A GENEROUS ONE. The budget is
        // checked before each poll AND the poll is clamped to what is left, so the
        // only overshoot is the scheduling delay in returning from the final poll.
        // 500ms covers that on a loaded box; a multiple of the share would not
        // distinguish this bound from one twice its size.
        assert!(
            elapsed < share + Duration::from_millis(500),
            "the writer waited {elapsed:?} against a {share:?} share: the wait is \
             overshooting its own bound, which is how a per-call and a per-process \
             budget both looked correct before"
        );
        // And it must not give up early, or the diagnostic is dropped on a reader
        // that was merely slow -- the failure the wait exists to avoid.
        assert!(
            elapsed >= share,
            "the writer gave up after {elapsed:?} with a {share:?} share still \
             unspent, so a slow reader would lose diagnostics it should have got"
        );
    }

    /// The share must shrink with nesting depth, or the tree total is unbounded.
    #[test]
    fn the_share_halves_at_each_nesting_level() {
        let tree = STDERR_TREE_WAIT_BUDGET.as_millis() as u64;
        // ⚠️ CALLS THE REAL ARITHMETIC. Recomputing the shift here instead left this
        // test green against a build with the halving deleted -- it compared a copy
        // of the formula with itself. `stderr_share_for_depth` is the function the
        // writer actually uses, evaluated at depths this process is not running at.
        let share_at = |depth: u32| stderr_share_for_depth(depth).as_millis() as u64;

        assert_eq!(share_at(0), 2_500, "the outermost hermit takes half the ceiling");
        assert_eq!(share_at(1), 1_250, "a nested hermit takes half of that");
        assert_eq!(share_at(2), 625);

        // ⚠️ THE PROPERTY THAT MATTERS IS THE SUM, NOT ANY ONE SHARE. Two hermit
        // processes at a full budget each was the defect; the sum over a chain of
        // ANY depth has to stay under one ceiling.
        let total: u64 = (0..16).map(share_at).sum();
        assert!(
            total < tree,
            "shares over a 16-deep chain sum to {total}ms, at or past the {tree}ms \
             invocation ceiling"
        );
    }

    /// ⚠️ THE NESTED HERMIT IS PID 1 IN ITS OWN NAMESPACE AND MUST NOT READ AS
    /// OUTERMOST. This is the case the /proc walk cannot be put into from a unit
    /// test, and the one that was wrong: measured `NSpid: 1455601 1` for the inner
    /// hermit of a real run, so from inside that namespace `/proc/1/stat` is itself
    /// and reports parent 0. Treating that as depth 0 gave BOTH processes the
    /// outermost share -- 2.5s + 2.5s = 5s, exactly the ceiling instead of under
    /// it, which is the very defect this change removes.
    #[test]
    fn the_container_init_does_not_read_as_the_outermost_hermit() {
        // Inside the container's pid namespace: we are pid 1 and there is no
        // parent to see. The real /proc would answer exactly this way.
        let depth = hermit_nesting_depth_of(1, |_| Some(0), |_| false);
        assert!(
            depth >= 1,
            "a hermit that is pid 1 in its own namespace reported depth {depth}, so \
             it would take the OUTERMOST share and the tree total would reach the \
             ceiling instead of staying under it"
        );
        // And its share must actually be smaller than the outermost one, or the
        // depth is right and the arithmetic still is not.
        assert!(
            stderr_share_for_depth(depth) < stderr_share_for_depth(0),
            "the nested share is not smaller than the outermost share"
        );
    }

    /// A real chain of hermit processes still counts levels normally.
    #[test]
    fn an_ordinary_hermit_chain_counts_its_levels() {
        // 100 -> 101 -> 102, all named hermit, then 103 which is not.
        let parent_of = |pid: u32| match pid {
            100 => Some(101),
            101 => Some(102),
            102 => Some(103),
            _ => Some(0),
        };
        let is_hermit = |pid: u32| matches!(pid, 101 | 102);
        assert_eq!(hermit_nesting_depth_of(100, parent_of, is_hermit), 2);
        // And the outermost, whose parent is a shell, is still depth 0.
        assert_eq!(hermit_nesting_depth_of(100, |_| Some(999), |_| false), 0);
    }

    /// A process whose parent is not `hermit` is the outermost one.
    #[test]
    fn the_nesting_walk_reports_zero_outside_a_hermit_tree() {
        // The test binary is not named `hermit`, so the walk must stop immediately.
        // If this ever reports nonzero, the walk is matching something it should
        // not and every share below the top is wrong.
        assert_eq!(hermit_nesting_depth(), 0);
    }

    /// The parent pid must survive a process name containing spaces and brackets.
    #[test]
    fn the_stat_parser_reads_past_a_comm_with_spaces() {
        // Field 2 of /proc/<pid>/stat is `(comm)` and comm may contain spaces and
        // parentheses, so a whitespace split misreads the parent pid. Parse our own
        // stat and cross-check against the value the kernel reports elsewhere.
        let ours = proc_stat_ppid(std::process::id()).expect("read our own ppid");
        let expected = unsafe { libc::getppid() } as u32;
        assert_eq!(
            ours, expected,
            "the /proc/<pid>/stat parse disagrees with getppid(), so the nesting \
             walk is reading the wrong field"
        );
    }
}
