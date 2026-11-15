---
schema: syscall-pages/v1
syscall: rt_sigpending
nr_x86_64: 127
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/406
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigpending&body=Discussion+for+the+%60rt_sigpending%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigpending.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigpending%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

Returns the set of signals pending (blocked and raised) for the caller.

Hermit forwards the call to the kernel unchanged (`detcore/src/syscalls/signal.rs:822-828`) and makes the answer repeatable by controlling what changes the pending set: guest signal sends and timer signals are handled by Detcore and happen at scheduled points, and Reverie's internal timer signal is kept out of the guest's blocked mask so it can never appear in the result.

Manual reference: [sigpending(2)](https://man7.org/linux/man-pages/man2/sigpending.2.html).

## Linux semantics

Reports signals raised while blocked. Verified: with nothing pending it returns an empty set (0).

`rt_sigpending(set, sigsetsize)` (kernel `kernel/signal.c`, `do_sigpending`) computes the union of the calling thread's own pending signals and its process's shared pending signals, intersected with the thread's blocked mask, and copies `sigsetsize` bytes of that mask to `set`. A signal that is pending but not blocked is about to be delivered and is not reported. The kernel rejects `sigsetsize` larger than the kernel `sigset_t` (8 bytes on x86_64) with `EINVAL`; a smaller size copies only the first `sigsetsize` bytes, and a size of 0 copies nothing and succeeds whatever `set` is. A bad `set` pointer with a non-zero size fails with `EFAULT`. The glibc `sigpending` wrapper always passes 8.

Bits are set when a signal is generated while blocked and cleared when it is consumed: delivery after unblocking, `sigwaitinfo`/`sigtimedwait`, a `signalfd` read, or setting the disposition to `SIG_IGN`. Standard signals coalesce (several raises leave one bit), real-time signals queue but still show as one bit. The pending set survives `execve`; a `fork` child starts with an empty pending set.

### Flags and options

- F1: valid `set` with `sigsetsize == 8`: writes (thread-pending OR process-pending) AND blocked, and returns 0.
- F2: `sigsetsize > 8`: `EINVAL`, nothing written.
- F3: `sigsetsize < 8`: copies only the first `sigsetsize` bytes of the mask; `sigsetsize == 0` writes nothing and succeeds even with a NULL `set`.
- F4: invalid `set` pointer (NULL or unmapped) with a non-zero `sigsetsize`: `EFAULT`.
- F5: how bits change: set by generation while blocked (`kill`, `tgkill`, `tkill`, `raise`, `rt_sigqueueinfo`, timer expiry, kernel-generated signals), cleared by consumption (delivery after unblocking, `sigwaitinfo`/`sigtimedwait`, `signalfd` read, `SIG_IGN`); standard signals coalesce; kept across `execve`, empty in a new `fork` child.
- F6: a pending signal that is not blocked is not reported.

## Determinism analysis

Which signals are pending depends on delivery timing from other threads/processes and the scheduler, so the returned set can vary run-to-run unless signal delivery is determinized.

`linux_determinism: nondeterministic`, kept from the legacy classification. Natively the answer is a snapshot of state that other threads, other processes, timers and the kernel change asynchronously, so two runs of the same program can read different sets at the same program point. The probe under Additional information only shows the trivial empty case.

### Nondeterminism sources

The pending-signal set reflects asynchronous delivery timing (scheduling-dependent).

- Signals sent by sibling threads or other processes, whose arrival relative to the read depends on scheduling
- Timer expiry (`alarm`, `setitimer`, POSIX timers) measured against real time
- Kernel-generated signals tied to external events: `SIGCHLD` from child exits, `SIGPIPE`, `SIGIO`/`SIGURG` for descriptors with an owner, `SIGXCPU`/`SIGXFSZ` resource limits
- Signals sent from outside the program entirely

## Hermit design

Target policy: `model`. The kernel keeps the pending set and Detcore forwards the read; what Detcore models is the set of events that change it, so the read happens at a scheduled point against state that was itself produced in a deterministic order. Emulating the read would mean tracking every pending bit in Detcore, which the code does not do and which would have to stay in step with the kernel's real delivery.

At hermit f4055eb9cae4:

- `handle_rt_sigpending` forwards the call through `record_or_replay` (`detcore/src/syscalls/signal.rs:827`), whose doc comment states the premise: "Read the kernel pending-signal mask after Detcore has serialized all signal generation and delivery events that can change it." (`detcore/src/syscalls/signal.rs:820-821`). In `hermit run` the inner tool only injects the call (`detcore/src/record_or_replay.rs:40-48`), so the kernel validates F2 to F4 itself.
- Guest-generated signals go through Detcore handlers: `kill`, `tgkill`, `tkill`, `rt_sigqueueinfo` and `rt_tgsigqueueinfo` (`detcore/src/lib.rs:2732-2744`), which run at the sender's scheduled turn.
- With the deterministic scheduler on (`sequentialize_threads`), `alarm` and `setitimer(ITIMER_REAL)` are registered on Detcore logical time instead of the host clock (`detcore/src/syscalls/signal.rs:270-278`, `detcore/src/syscalls/signal.rs:304-313`), so when `SIGALRM` becomes pending is a function of the schedule. Other interval timers are refused with `ENOSYS` (`detcore/src/syscalls/signal.rs:304-306`).
- Reverie uses a signal of its own for preemption timers. Detcore strips it from every mask the guest installs, both in `rt_sigaction` masks and in `rt_sigprocmask` sets (`detcore/src/syscalls/signal.rs:126-129`, `detcore/src/syscalls/signal.rs:484`, `detcore/src/syscalls/signal.rs:512`). Because `rt_sigpending` reports only blocked signals (F6), that internal signal can never appear in the result.
- Detcore itself issues `rt_sigpending` inside `handle_rt_sigsuspend` to decide whether a signal is already pending before the temporary mask is installed (`detcore/src/syscalls/signal.rs:414-421`), so the same kernel query also drives a scheduling decision.

Gaps that keep this short of a complete model:

- **Without the deterministic scheduler** (`sequentialize_threads` off), `alarm` and `setitimer` go to the host clock (`detcore/src/syscalls/signal.rs:279-285`, `detcore/src/syscalls/signal.rs:297-303`), so `SIGALRM` pending state depends on real time.
- **Signals from outside the container and host-event signals** (`SIGIO`/`SIGURG` through descriptor ownership, `SIGXCPU`/`SIGXFSZ`) are not generated by any Detcore handler. Whether each is deterministic under Hermit has not been checked here; listing them is a candidate set, not a finding.
- **Record and replay do not record the result.** The recorder has no arm for `rt_sigpending`, so it falls to the default that only injects (`hermit-cli/src/recorder.rs:648`), and the replayer re-executes it the same way (`hermit-cli/src/replayer.rs:575`). Replay therefore reads the live kernel set, which is correct only if replay regenerates the same signals in the same order.
- **sigsetsize is not checked by Detcore.** Unlike `rt_sigprocmask` and `rt_sigsuspend`, which require exactly 8 bytes (`detcore/src/syscalls/signal.rs:118-124`, used at `detcore/src/syscalls/signal.rs:403` and `detcore/src/syscalls/signal.rs:505`), `rt_sigpending` leaves F2 and F3 to the kernel. That matches Linux, where `rt_sigpending` alone accepts a short size.

## Recommended tests

- T1 (covers F1, F5, F6): block `SIGUSR1`, raise it several times, check `sigpending` shows it, check that an unblocked pending signal is not reported, then unblock and check exactly one delivery and an empty set afterwards. Run under `hermit run --verify` on ptrace.
  Existing partial coverage: `tests/c/signal_delivery_sequence.c:435-453` does the block, raise, `sigpending` and unblock sequence (cell `c-programs/signal-delivery-sequence`, `tests/e2e/manifests/c-programs.yaml:17172`, verify ci on ptrace at `tests/e2e/manifests/c-programs.yaml:17189-17191`). `pending_signal_completes_sigsuspend_and_restores_mask` (`hermit-cli/tests/signal_determinism.rs:308`) runs the `pending-sigsuspend` scenario, which checks `SIGUSR1` is pending before `sigsuspend` and reads the set again after it (`tests/c/signal_determinism.c:520-541`), five runs on the default backend with identical output required.
- T2 (covers F2, F3, F4): call the raw syscall with `sigsetsize` 16 (`EINVAL`), 4 (only the low 4 bytes written, the rest of a prefilled buffer untouched), 0 with a NULL `set` (returns 0), and 8 with NULL and with a `PROT_NONE` page (`EFAULT`); compare with native Linux.
- T3 (covers F5): timer-generated pending state: block `SIGALRM`, arm `setitimer(ITIMER_REAL)`, poll `sigpending` until it is set, and require the same poll count and output on every run.
  Existing partial coverage: `sigalrm_itimer_delivery_is_deterministic` (`hermit-cli/tests/signal_determinism.rs:231`) runs the `itimer-delivery` scenario (`tests/c/signal_determinism.c:71-111`) five times; the manifest cell `c-programs/signal-determinism` (`tests/e2e/manifests/c-programs.yaml:8132`) runs the same scenario under verify on ptrace (`tests/e2e/manifests/c-programs.yaml:8159-8165`). Neither prints the poll count.
- T4 (covers F5): consumption and inheritance: pending `SIGUSR1` survives `execve` (existing: `pending_signal_and_mask_survive_exec`, `hermit-cli/tests/signal_determinism.rs:408`, scenario at `tests/c/signal_determinism.c:841-867`); a `fork` child sees an empty set; `sigtimedwait` and a `signalfd` read clear the bit (partial existing check for `signalfd` and `SIGPIPE`: `tests/c/sigpipe_siginfo.c:154-158`, cell `c-programs/sigpipe-siginfo` at `tests/e2e/manifests/c-programs.yaml:8207`).
- T5 (covers F1, F5): two threads, one sending `pthread_kill` to the other while the target polls `sigpending` with the signal blocked; run under `--verify` and under record and replay on ptrace and require the same poll iteration to observe the bit in every run.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/406: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel keeps the set; Detcore orders guest sends and logical-time alarms and hides Reverie's timer signal); linux_determinism stays nondeterministic; 6 flag behaviours and 5 tests listed; record/replay does not record the result (noted as a gap); tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `rt_sigpending`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+rt_sigpending&body=Discussion+for+the+%60rt_sigpending%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Frt_sigpending.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60rt_sigpending%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: rt_sigpending" (https://github.com/rrnewton/hermit/issues/406), legacy variant A1.

The legacy recommendation was NEEDS-HANDLER. A handler now exists (`detcore/src/syscalls/signal.rs:822`), so that recommendation is superseded; what remains is the gap list in Hermit design.

The Linux semantics above are from the kernel source (`kernel/signal.c`) as known to the author; no man page or kernel source was installed on the host used for this pass, so T2 should confirm the size handling.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=rt_sigpending` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `rt_sigpending` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
rt_sigpending([], 8)            = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
