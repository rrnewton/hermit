---
schema: syscall-pages/v1
syscall: tkill
nr_x86_64: 200
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/499
  - https://github.com/rrnewton/hermit/issues/520
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tkill&body=Discussion+for+the+%60tkill%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftkill.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tkill%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 9
---

## Summary

`tkill(tid, sig)` is the obsolete two-argument form of `tgkill`: it sends a signal to one thread, named by thread ID only. Detcore models it (`detcore/src/syscalls/signal.rs:704`). The kernel performs the delivery, the call is issued only when the deterministic scheduler runs the sender, its result goes through record and replay, and a successful send to another thread is reported to the scheduler so a parked target is woken.

Manual reference: [tkill(2)](https://man7.org/linux/man-pages/man2/tgkill.2.html).

## Linux semantics

`tkill` looks up `tid` in the caller's PID namespace and queues `sig` on that thread. It is deprecated in favor of `tgkill(tgid, tid, sig)`, which Linux documents as the safer replacement: `tkill` does not verify the thread-group ID, so if the intended thread exits and its ID is reused, the signal reaches an unrelated thread, possibly in another process. glibc has no wrapper; programs call it through `syscall(SYS_tkill, ...)`.

The kernel checks arguments in this order:

1. `tid <= 0` is `EINVAL`.
2. The thread is looked up; if none has that ID in the caller's PID namespace, the result is `ESRCH`.
3. `sig` must be 0 or a valid signal number (1 to 64 on x86_64); otherwise `EINVAL`. Because the lookup comes first, an invalid signal sent to a missing thread gives `ESRCH`.
4. The caller needs permission to signal the target: the same user, or `CAP_KILL`; otherwise `EPERM`. A security module can also refuse.

Signal 0 performs the checks and queues nothing, so it is an existence and permission probe; both host probes under Additional information sent signal 0 to the calling thread and got 0.

A thread-directed signal goes on the target thread's private pending set. A standard signal (1 to 31) that is already pending is not queued twice. A real-time signal (34 to 64) is queued every time, with `si_code` `SI_TKILL`, until the real user's `RLIMIT_SIGPENDING` is reached, after which the send fails with `EAGAIN`. Delivery to another thread is asynchronous: `tkill` returns before the handler runs, and a target blocked in an interruptible system call is woken with `EINTR` or a restart. A signal sent to the calling thread and not blocked is delivered before `tkill` returns to user space. Signals whose action is to stop, continue or kill (`SIGKILL`, `SIGSTOP`, `SIGCONT`, and default-fatal signals) act on the whole thread group even though one thread was named.

### Flags and options

- F1: `sig == 0`: no signal is sent; returns 0 if the thread exists and the caller may signal it, otherwise the error of the failed check.
- F2: a standard signal sent to the calling thread: delivered before the call returns unless blocked; a second copy while one is pending is merged.
- F3: a signal sent to another thread of the same process: queued on that thread and delivered asynchronously; a target blocked in an interruptible call is woken with `EINTR` or a restart.
- F4: a signal sent to a thread of another process, which `tkill` permits because it does not check the thread-group ID.
- F5: a real-time signal: queued once per send with `si_code` `SI_TKILL`, `si_pid` and `si_uid` of the sender; `EAGAIN` once `RLIMIT_SIGPENDING` is reached.
- F6: `SIGKILL`, `SIGSTOP`, `SIGCONT` and default-fatal signals: they act on the whole thread group, including when the named thread is not the leader.
- F7: invalid signal number (negative, or above 64): `EINVAL` when the thread exists, `ESRCH` when it does not, because the lookup runs first.
- F8: `tid <= 0`: `EINVAL`.
- F9: no thread with that ID in the caller's PID namespace: `ESRCH`.
- F10: the caller may not signal the target (different user, no `CAP_KILL`): `EPERM`.
- F11: the target blocks or ignores the signal: the call still returns 0; a blocked signal stays pending, and an ignored one is discarded.
- F12: thread ID reuse: if the intended thread has exited and its ID is reused, the signal reaches the new thread, which may belong to another process.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from both legacy classifications. On the host the call shares `tgkill`'s nondeterminism: when the signal arrives relative to the target's progress depends on host scheduling, and the thread ID a program can name is the host's, reused without warning after the thread exits. The result itself (0, `ESRCH`, `EPERM`) depends on whether the target is still alive and on credentials and signal masks at the moment of the call. Programs must translate thread IDs and deliver deterministically to repeat.

### Nondeterminism sources

- Asynchronous delivery timing: where in its execution the target thread is when the signal arrives.
- Thread ID allocation and reuse: host thread IDs differ between runs (the host probes saw 3069897 and 3143669), and a reused ID retargets the signal.
- Target lifetime: whether the target has exited, which decides between 0 and `ESRCH`.
- Credentials, which decide `EPERM`.
- Signal masks and dispositions of the target, which decide whether the signal is delivered, kept pending or discarded.
- Concurrent signals from other senders, which decide merging of standard signals and the order of real-time signals.

## Hermit design

Target policy: `model`. The arm `Syscall::Tkill(s) => self.handle_tkill(guest, s)` (`detcore/src/lib.rs:2738`) forwards the call to the kernel, which keeps the pending sets and performs delivery, while Detcore owns the identities and the moment of every send. Code at hermit f4055eb9cae4:

- The guest runs in a fresh PID namespace, so the `tid` the guest passes is the guest's own stable thread ID, and the kernel translates it. The handler's doc comment (`detcore/src/syscalls/signal.rs:700-703`) states this and argues that, because delivery is scheduler-serialized, forwarding is deterministic.
- The call is issued through `record_or_replay` (`detcore/src/syscalls/signal.rs:717`), so the kernel's answer is recorded and replayed. A failed call returns its errno through `?` and skips the notification below.
- After a successful send, `notify_cross_task_signal` (`detcore/src/syscalls/signal.rs:720`, defined at `detcore/src/syscalls/signal.rs:734`) tells the scheduler that a signal is pending for the target, which wakes a target parked in a wait the scheduler owns (F3). It does so only when the signal is nonzero and the target is not the sender (`should_notify_cross_task_signal`, `detcore/src/syscalls/signal.rs:831-833`), so a probe (F1) and a self-send (F2) queue no wakeup. The comment at `detcore/src/syscalls/signal.rs:741-748` records why there is no `Signal::try_from` gate: it once dropped the notification for every real-time signal and left a waiting target hung, so real-time sends (F5) are notified like the rest. `kill`, `tgkill` and `tkill` share this function so the three cannot drift (`detcore/src/syscalls/signal.rs:728-733`).
- On the KVM backend, a `SIGKILL` addressed to the calling thread first reserves a group exit with the scheduler (`detcore/src/syscalls/signal.rs:709-716`, `reserve_kvm_self_sigkill_exit` at `detcore/src/syscalls/signal.rs:571`). On ptrace that reservation returns at once, because it requires `kvm_shared_dequeue_timers` (`detcore/src/syscalls/signal.rs:578`).

Why `model` and not the alternatives:

- `emulate` would mean reimplementing signal queueing, masks and group-wide actions in Detcore; the kernel already does that exactly once the sender's timing and the thread IDs are deterministic.
- `refuse` would break programs and runtimes that still issue `tkill` directly, and the Rust regression guest `tests/rust/tkill.rs` exists because `--strict` once aborted on it (`tests/rust/tkill.rs:9-12`).
- A plain forward without the notification would let a target parked in a scheduler-owned wait miss the signal and hang, the failure the comment at `detcore/src/syscalls/signal.rs:728-733` describes.

What the code does not settle:

- **Thread ID reuse (F12).** Detcore does not refuse a `tkill` to a thread ID that belonged to an exited thread; the kernel resolves the ID at the moment of the call. Inside the guest PID namespace the allocation order is set by the deterministic schedule, so reuse repeats from run to run, but it can still retarget a signal exactly as on Linux. Hermit makes the race deterministic; it does not remove it.
- **Threads outside the guest (F4, F10).** Guest IDs are confined to the guest PID namespace, so a thread outside the container cannot be named and gives `ESRCH`. Credentials inside the namespace are the guest's, so `EPERM` from a different-user target needs a guest that changes user.

`linux_determinism` stays `nondeterministic`: it describes the raw call on Linux. Under Hermit the timing and identity sources are removed by the scheduler and the PID namespace.

## Recommended tests

- T1 (covers F2): a single-threaded guest sends `SIGUSR1` to itself with raw `syscall(SYS_tkill, gettid(), SIGUSR1)` and checks the handler has run when the call returns.
  Existing candidate: `tests/rust/tkill.rs:107-115`, built as the cargo binary `rustbin_tkill` (`tests/Cargo.toml:187-188`) and listed in `tests/BUCK:378`. No manifest cell or `hermit-cli` test runs it at this SHA (Open questions), so it does not yet count as running on ptrace in CI.
- T2 (covers F1, F7, F8, F9): under `hermit run --verify` on ptrace, check `tkill(gettid(), 0) == 0`; `tkill(0, SIGUSR1)` and `tkill(-1, SIGUSR1)` give `EINVAL`; `tkill(gettid(), 65)` and `tkill(gettid(), -1)` give `EINVAL`; a thread ID that was never allocated gives `ESRCH` for signal 0, `SIGUSR1` and 65.
- T3 (covers F3, F11): a thread blocked in `read` on an empty pipe, with a handler installed without `SA_RESTART`, is woken by `tkill` from a sibling and sees `EINTR`; repeat with the signal blocked in the target and check it stays pending (`sigpending`) and is delivered on unblock; repeat with `SIG_IGN` and check the call returns 0 and nothing is delivered. Run under `--verify` so the wakeup order must repeat.
- T4 (covers F5): send `SIGRTMIN` three times to a sibling thread that blocks it, then unblock and count three deliveries with `si_code == SI_TKILL` and `si_pid` equal to the sender's process ID; send a standard signal three times and count one.
- T5 (covers F6): `tkill(nonleader_tid, SIGKILL)` from a non-leader thread ends the whole process with signal 9.
  Existing candidate on KVM only: the `thread-tkill` case of `tests/c/kvm_exact_child_waits.c:172` (raw `SYS_tkill` at `tests/c/kvm_exact_child_waits.c:120`), run by `run_kvm_self_sigkill_from_nonleader_is_group_fatal` at `hermit-cli/tests/cli.rs:3218` with `--backend=kvm`. A ptrace run of the same guest is still needed.
- T6 (covers F4, F10): a parent sends `SIGUSR1` with `tkill` to a thread of its forked child, which the child observes; then a child that changed its user ID with `setresuid` gets `EPERM` from `tkill` on its parent's thread, if the guest's user namespace allows the change.
- T7 (covers F12): a thread exits and is joined; the program spawns new threads until one reuses the old ID, or a bounded number has been created, then sends `tkill` to the old ID. Under `--verify` both runs must report the same outcome (delivery to the new thread, or `ESRCH`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/499: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/520 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (the kernel delivers; Detcore issues the call in scheduler order through record_or_replay and notifies the scheduler of every successful cross-thread send, real-time signals included); linux_determinism stays nondeterministic; merged duplicate https://github.com/rrnewton/hermit/issues/520 and reconciled its stale pass-through claim; 12 flag behaviours and 7 tests listed; open question: the existing guest tests/rust/tkill.rs has no ptrace runner, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `tkill`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+tkill&body=Discussion+for+the+%60tkill%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftkill.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60tkill%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: tkill" (https://github.com/rrnewton/hermit/issues/499), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/520, filed the same day under the same title (legacy variant A1), is merged into the sections above. Its comment history (0 comments) stays on that issue, as does that of https://github.com/rrnewton/hermit/issues/499 (0 comments).

### Open questions

- `tests/rust/tkill.rs` is built as `rustbin_tkill` and listed in `tests/BUCK:378`, but no manifest cell and no `hermit-cli` test runs it: the cargo-binary list at `hermit-cli/tests/hermit_modes.rs:897` onward does not include it. The inventory entry in `tests/e2e/manifests/inventory/test-files.json:3717-3722` says a Rust integration test selects it, which this search did not confirm. It needs a ptrace runner before T1 can count.
- The only multi-thread `tkill` test (`tests/c/kvm_exact_child_waits.c`, through `hermit-cli/tests/cli.rs:3218`) runs on KVM only. Should the same guest also run on ptrace for T5?

### Reconciled claims from the legacy texts

- Both legacy texts say Hermit routes `tkill` through the temporary pass-through block of `detcore/src/syscall_classification.rs`, which was true at `dd60278f`. At hermit f4055eb9cae4 the typed arm at `detcore/src/lib.rs:2738` sends it to `handle_tkill`, which forwards through `record_or_replay` and adds the scheduler notification. The code wins, and Hermit design describes it.
- Both texts say the thread ID must be virtualized or translated. At this SHA that is done by the fresh PID namespace rather than by Detcore rewriting the argument (`detcore/src/syscalls/signal.rs:700-703`). Thread ID reuse remains possible, but repeats under the deterministic schedule (Hermit design).
- Both texts recommended NEEDS-HANDLER. The handler exists; the policy it realizes is `model`.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/499)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=tkill` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `tkill` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
tkill(3069897, 0)               = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/520)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=tkill`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `tkill` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
tkill(3143669, 0)               = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
