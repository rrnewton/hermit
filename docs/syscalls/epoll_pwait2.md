---
schema: syscall-pages/v1
syscall: epoll_pwait2
nr_x86_64: 441
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/345
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait2&body=Discussion+for+the+%60epoll_pwait2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 7
---

## Summary

`epoll_pwait2(epfd, events, maxevents, timeout, sigmask, sigsetsize)` is `epoll_pwait` with a `struct timespec *` timeout instead of an `int` count of milliseconds. Hermit gives it a scheduler turn and then forwards the raw call through record/replay. That matches only the non-NULL-sigmask arm of its `epoll_pwait` handler, so a blocking wait is not yet integrated with the scheduler.

## Linux semantics

Added in Linux 5.11. The call waits until at least one descriptor in the epoll interest set is ready, the timeout expires, or a signal handler runs. It returns up to `maxevents` ready events. If `sigmask` is non-NULL, the kernel installs it as the thread's signal mask for the duration of the wait and restores the old mask afterwards, atomically with respect to signal delivery. The call is never restarted after a signal handler, whatever `SA_RESTART` says.

### Flags and options

- F1: NULL `timeout`: wait without a time limit.
- F2: Zero `timeout` (`{0, 0}`): poll the ready list and return at once, 0 if nothing is ready.
- F3: Positive `timeout`: wait up to that many nanoseconds, measured on `CLOCK_MONOTONIC` and rounded up to the clock's granularity; returns 0 if it expires with nothing ready.
- F4: NULL `sigmask`: the signal mask is unchanged.
- F5: Non-NULL `sigmask`: installed for the duration of the wait and restored afterwards; `sigsetsize` must equal the kernel sigset size (8 on x86-64), otherwise `EINVAL`.
- F6: Argument errors: `maxevents` of 0 or less, or above the kernel maximum, returns `EINVAL`; an `epfd` that is not an epoll descriptor returns `EINVAL`, and a closed one `EBADF`; a timespec with a negative field or `tv_nsec` of 1000000000 or more returns `EINVAL`; an unreadable `timeout` or `sigmask`, or an unwritable `events` buffer, returns `EFAULT`.
- F7: Signal interruption: if a handler runs during the wait, the call returns `EINTR`.
- F8: Returned events: each entry carries the ready mask and the `data` value registered with `epoll_ctl`, following that registration's level-triggered, `EPOLLET` or `EPOLLONESHOT` mode.

## Determinism analysis

`linux_determinism: nondeterministic`. Natively, what the call returns depends on when other threads and processes make descriptors ready, relative to the wait.

### Nondeterminism sources

- Asynchronous descriptor readiness: the count and set of ready events depends on how far producers have got when the wait runs.
- Timeout expiry (F3) is measured on host time.
- Signal arrival relative to the wait decides between `EINTR` and a normal return, and under F5 the mask swap decides which signals can arrive.
- The order of entries on the ready list follows the order in which descriptors became ready.

The call repeats only when readiness, signal delivery and timeout expiry are ordered by a deterministic scheduler and virtual time, or for F2 on descriptors whose readiness is already fixed.

## Hermit design

Target policy: `model`. Detcore should treat `epoll_pwait2` like `epoll_pwait`: wait for readiness inside the deterministic scheduler, with the timeout counted in virtual time, rather than blocking in the kernel.

What the code does at hermit f4055eb9cae4:

- Classification: determinized (`detcore/src/syscall_classification.rs:84`); the comment says it is "handled identically to epoll_pwait" (`detcore/src/syscall_classification.rs:78-83`).
- Dispatch: the pinned Reverie has no typed variant, so a Sysno-guarded arm calls `handle_epoll_pwait2` (`detcore/src/lib.rs:2354-2356`; comment at `detcore/src/lib.rs:2349-2353`).
- Handler: an empty resource request (a scheduler turn), then `record_or_replay` of the raw call, whatever the timeout or mask (`detcore/src/syscalls/io.rs:1284-1292`).

How that differs from `epoll_pwait` at the same SHA: `handle_epoll_pwait` does exactly this only for a non-NULL sigmask (`detcore/src/syscalls/io.rs:1217-1221`). For a NULL sigmask it uses a non-blocking path: a zero-timeout injection after a scheduler request, or a non-blocking retry loop that holds `InternalIOPolling` against a virtual-time deadline (`detcore/src/syscalls/io.rs:1238-1271`), or `record_or_replay_blocking` in record/replay or non-sequentialized modes (`detcore/src/syscalls/io.rs:1222-1230`). The comment at `detcore/src/syscalls/io.rs:1192-1200` records why: a raw blocking inject under `--sequentialize-threads` deadlocks the guest, because the waiting task holds the scheduler turn while the only task that could satisfy the wait sits in the run queue.

Why model: epoll waits are the core of event loops, and a raw forward of a blocking wait is the observed `cmake` deadlock above. Refusing would break the programs that call it directly. `epoll_pwait` already has the model for the NULL-sigmask case, so the work is reusing it with a timespec deadline.

Known gaps:

- F1 and F3: a blocking `epoll_pwait2` is injected raw and can hold the scheduler turn, the same deadlock as the comment at `detcore/src/syscalls/io.rs:1192-1200` describes for `epoll_pwait`. Its timeout runs on host time, not virtual time.
- F5 and F7: a non-NULL sigmask has the same remaining gap that `epoll_pwait` documents (`detcore/src/syscalls/io.rs:1212-1216`).
- The comments at `detcore/src/lib.rs:2349-2353` and `detcore/src/syscalls/io.rs:1273-1281` say the handling is identical to `epoll_pwait`; it matches only the non-NULL-sigmask arm.
- The only test uses a zero timeout and a NULL sigmask.

Target (owner decisions 6, 13, 14): a wait that could block is a zero-timeout probe plus a scheduler wait under `InternalIOPolling`, like `handle_internal_epoll_pwait` (`detcore/src/syscalls/io.rs:1238-1271`), but with an absolute virtual-time deadline built from the timespec instead of from milliseconds. A non-NULL mask rides on every probe after `sanitize_ppoll_signal_mask`, as `pselect6` does (`detcore/src/syscalls/io.rs:614-625`). Because Detcore must read the timespec to build the deadline, it owes Linux's checks in the kernel's order: `SYSCALL_DEFINE6(epoll_pwait2)` in `fs/eventpoll.c` copies the timespec (`EFAULT`) and validates it with `poll_select_set_timeout` (`EINVAL` for a negative field or `tv_nsec` of 1000000000 or more) before anything else; then `do_epoll_pwait` installs the mask (`set_user_sigmask`: `EINVAL` for a bad `sigsetsize`, `EFAULT`); then `do_epoll_wait` checks `maxevents` (`EINVAL`), the `events` range (`EFAULT`), `epfd` (`EBADF`) and that it is an epoll file (`EINVAL`). Detcore validates the timespec itself and leaves the rest to the first probe, which reaches the kernel in that order. Today every call takes one turn and is then forwarded raw (`detcore/src/syscalls/io.rs:1284-1292`), so a blocking wait holds the turn in the kernel and its timeout runs on host time. Recent glibc reaches `epoll_pwait2` from `epoll_wait` where the kernel has it (comment at `detcore/src/syscalls/io.rs:1275-1277`), so it is not refused while the probe form is built.

## Recommended tests

- T1 (covers F2, F4, F8): An eventfd registered for `EPOLLIN` and written once is reported once with its `data.fd` and `EPOLLIN`; after `EPOLL_CTL_DEL` a zero-timeout wait returns 0. Existing candidate: `tests/c/epoll_pwait2_readiness.c` (7 checks, zero timespec, NULL sigmask; cell `c-programs/epoll-pwait2` at `tests/e2e/manifests/c-programs.yaml:11446`, verify CI at `tests/e2e/manifests/c-programs.yaml:11464` with ptrace enabled at `tests/e2e/manifests/c-programs.yaml:11466`).
- T2 (covers F1, F8): One thread waits with a NULL timeout on an eventfd and a sibling thread writes it later. Under `--strict --verify` the guest finishes without deadlock and both runs report the same event.
- T3 (covers F3): A 50 ms timeout with nothing ready returns 0 and the `CLOCK_MONOTONIC` time elapsed across the call is identical in both `--verify` runs; a second wait with a 10 s timeout is satisfied by a sibling's write and returns 1.
- T4 (covers F5, F7): With `SIGUSR1` blocked, a wait whose `sigmask` unblocks it returns `EINTR` when a sibling sends `SIGUSR1` with `tgkill`; the handler ran once and the original mask is back afterwards. `sigsetsize` of 4 returns `EINVAL`.
- T5 (covers F6): `maxevents` of 0 and of -1, a regular-file `epfd`, a closed `epfd`, `tv_nsec` of 1000000000, and a NULL `events` buffer with a ready descriptor each return the Linux errno.
- T6 (covers F1, F3, F8): Record and replay T2 and T3; the replay reports the same events and return values.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/345#issuecomment-5872964838
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/345: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch sysno-guard.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; epoll_pwait2 records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/345 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (handle_epoll_pwait2 at detcore/src/syscalls/io.rs:1284-1292 forwards the raw call after a scheduler turn, matching only epoll_pwait's non-NULL-sigmask arm, so blocking and positive timeouts are not scheduler-integrated); linux_determinism stays nondeterministic; 8 flag behaviours and 6 tests listed; only T1 has an existing candidate (tests/c/epoll_pwait2_readiness.c), no coverage token recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6, 13 and 14 applied (Target paragraph: probe-plus-wait with a virtual-time timespec deadline and the mask on each probe; Detcore validates the timespec first, then the kernel's set_user_sigmask and do_epoll_wait order; today the raw call is forwarded and blocks holding the turn); target_policy stays model; no new F/T ids; no open questions existed
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/epoll_pwait2_readiness.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_pwait2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_pwait2&body=Discussion+for+the+%60epoll_pwait2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_pwait2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_pwait2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: epoll_pwait2" (https://github.com/rrnewton/hermit/issues/345), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/345.

The legacy summary placed this syscall at rank 66 in the batch-2 classification sequence, after the 50 workload-observed batch-1 syscalls, and pointed to the Linux man page epoll_pwait2(2). The legacy recommended action was NEEDS-HANDLER.

The comment at `detcore/src/lib.rs:2351-2352` says recent glibc routes `epoll_wait` and `epoll_pwait` through `epoll_pwait2`; the comment at `detcore/src/syscalls/io.rs:1202-1207` reports that `strace` of glibc 2.34 on x86-64 shows a plain `epoll_wait`. Neither claim was re-measured here.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=epoll_pwait2 -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(441, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
epoll_pwait2(-1, 0xffffffffffffffff, -1, 0xffffffffffffffff, 0xffffffffffffffff, 18446744073709551615) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/345#issuecomment-5872964838
