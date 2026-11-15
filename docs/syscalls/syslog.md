---
schema: syscall-pages/v1
syscall: syslog
nr_x86_64: 103
linux_determinism: nondeterministic
target_policy: emulate
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/495
  - https://github.com/rrnewton/hermit/issues/516
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+syslog&body=Discussion+for+the+%60syslog%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsyslog.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60syslog%60%2C+or+in+what+the+page+says%3F%0A
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

The raw `syslog(type, bufp, len)` system call (glibc `klogctl`, not the C library's syslog(3)) reads or controls the host kernel's message ring buffer, the log `dmesg` shows. Hermit never forwards it: Detcore presents an empty virtual ring buffer (`handle_syslog`, `detcore/src/syscalls/misc.rs:431` at hermit f4055eb9cae4). Every valid action returns 0, control actions are inert, and invalid actions get `EINVAL`. See [syslog(2)](https://man7.org/linux/man-pages/man2/syslog.2.html).

## Linux semantics

`type` selects one of eleven actions. `bufp` and `len` are used only by the read actions and, for `len` alone, by `SYSLOG_ACTION_CONSOLE_LEVEL`; the kernel treats `type` and `len` as `int`. There is one ring buffer per host. It is not namespaced, so every container on the host reads and clears the same log.

The kernel checks permission before it looks at the action. When the `kernel.dmesg_restrict` sysctl is 0, `SYSLOG_ACTION_READ_ALL` and `SYSLOG_ACTION_SIZE_BUFFER` are open to every caller. Every other action, and every action when `dmesg_restrict` is 1, needs `CAP_SYSLOG` in the initial user namespace, so root inside a user namespace gets `EPERM`. A Linux security module can refuse any action. Because the check comes first, an unprivileged caller gets `EPERM` even for an invalid action.

### Flags and options

- F1: `SYSLOG_ACTION_CLOSE` (0) and `SYSLOG_ACTION_OPEN` (1): no-ops that return 0
- F2: `SYSLOG_ACTION_READ` (2): destructive read. `bufp == NULL` or `len < 0` gives `EINVAL`; `len == 0` returns 0; an inaccessible buffer gives `EFAULT`. Otherwise the call sleeps until the log holds unread data, then copies up to `len` bytes, consumes them, and returns the count. A signal ends the wait with `EINTR`, or restarts it under `SA_RESTART`.
- F3: `SYSLOG_ACTION_READ_ALL` (3): non-destructive copy of up to the last `len` bytes of the log; returns the count. It has the same `EINVAL`/0/`EFAULT` argument checks as F2 and never waits.
- F4: `SYSLOG_ACTION_READ_CLEAR` (4): like F3, then clears the log
- F5: `SYSLOG_ACTION_CLEAR` (5): clears the log for every later `READ_ALL` on the host; returns 0
- F6: `SYSLOG_ACTION_CONSOLE_OFF` (6) and `SYSLOG_ACTION_CONSOLE_ON` (7): save the host's console log level and lower it to the minimum, or restore the saved level; return 0
- F7: `SYSLOG_ACTION_CONSOLE_LEVEL` (8): `len` (as an `int`) must be 1 to 8, else `EINVAL`. A value below the minimum console level is raised to it. Sets the host's console log level, re-enables the console, and returns 0.
- F8: `SYSLOG_ACTION_SIZE_UNREAD` (9): returns the number of bytes not yet consumed by F2
- F9: `SYSLOG_ACTION_SIZE_BUFFER` (10): returns the total size of the ring buffer. It is never 0; the 2026-07-24 probe read 2097152 on the measurement host (Additional information).
- F10: any other `type`, negative or above 10: `EINVAL`, after the permission check
- F11: permission: `EPERM` without `CAP_SYSLOG` in the initial user namespace, except for F3 and F9 while `kernel.dmesg_restrict` is 0. An LSM can also refuse.

## Determinism analysis

`linux_determinism: nondeterministic`. Both legacy filings classified it so, and nothing here changes that. The log is host-global, and every part of the host writes into it at times no guest controls. What a read returns, how many bytes are unread, how long `SYSLOG_ACTION_READ` waits, and whether a call is permitted at all depend on that state. The control actions (F4-F7) also change host-global state that other processes observe. The call repeats only for invalid arguments that the kernel rejects before reading any state, and even then only when the permission outcome is the same.

### Nondeterminism sources

- Log contents: boot history and concurrent kernel messages from the whole host, arriving asynchronously (F2, F3, F4, F8)
- Buffer configuration: the size depends on the kernel's build and boot configuration (F9)
- Whether `SYSLOG_ACTION_READ` returns at once or waits, and for how long, depends on when the next host message arrives (F2)
- Permissions: the `kernel.dmesg_restrict` sysctl, the caller's capabilities and user namespace, and LSM policy (F11)
- Other processes on the host that clear the log or change the console level (F4-F7), which in turn change what later reads return

## Hermit design

Target policy: `emulate`. The code at hermit f4055eb9cae4 realizes an empty, stateless ring buffer:

- The typed dispatch arm `Syscall::Syslog(s) => self.handle_syslog(guest, s)` is at `detcore/src/lib.rs:2516`, tagged `TODO-HUMAN-REVIEW(PR-857)` with the comment "Empty virtual kernel ring buffer" (`detcore/src/lib.rs:2515`).
- `handle_syslog` (`detcore/src/syscalls/misc.rs:431`) ignores the guest's memory and returns `deterministic_syslog_result(call.priority(), call.len())` (`detcore/src/syscalls/misc.rs:436`). That function (`detcore/src/syscalls/misc.rs:172`) returns 0 for actions 0-7, 9 and 10 (`detcore/src/syscalls/misc.rs:176`), returns 0 for action 8 when `len` is 1 to 8 (`detcore/src/syscalls/misc.rs:177`), and `EINVAL` for everything else (`detcore/src/syscalls/misc.rs:178`).
- `syslog` is classified `Determinized` at `detcore/src/syscall_classification.rs:622`; the comment at `detcore/src/syscall_classification.rs:616` says it "exposes an empty ring buffer and never reads host dmesg state".
- Unit tests: `syslog_exposes_an_empty_log_and_validates_actions` (`detcore/src/syscalls/misc.rs:1106`) pins the function's results; `clock_discipline_and_kernel_log_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2175`) pins the classification; `passthru_opt_subscribes_every_determinized_syscall` asserts that record/replay delivers `syslog` to Detcore (`detcore/src/lib.rs:3166`), and `hermit-cli/src/metadata.rs:604` repeats that assertion on the CLI side. None of these starts a guest.

Why `emulate`:

- **It removes every host input.** Contents, size, unread count, waiting time and permission no longer depend on the host. Both legacy filings asked for a handler (**NEEDS-HANDLER**) because Hermit "must sanitize, record, or deterministically refuse access"; an empty log is the sanitizing answer.
- **It closes an isolation hole.** `SYSLOG_ACTION_CLEAR`, `READ_CLEAR` and the console actions never reach the host, so a guest cannot clear the host's log or change its console level.
- **It agrees with the fixed virtual-root identity.** The guest is root to itself (`getuid` returns 0, `detcore/src/lib.rs:2760`), and root with `CAP_SYSLOG` is allowed every action. So F11's `EPERM` never appears, whatever the host's `dmesg_restrict` says.
- `model` would need a virtual log with contents and a read cursor. Nothing inside the guest writes to the kernel log through Hermit, so an always-empty log is the complete state, and there is nothing left to model.
- `refuse` (a fixed `EPERM`, as for an unprivileged caller) would also be deterministic. It would contradict the virtual root that the rest of the credential family presents, and `dmesg`-style tools would report an error instead of an empty log.
- `passthrough` is what the legacy text described: the call reached the host log, and both 2026-07-24 probes read the host's 2 MiB buffer size through it.

Target (owner decision 6): the emulation owes Linux's checks in the order of the kernel's `do_syslog`. The syscall takes `type` and `len` as `int`, so `len` is truncated to its low 32 bits first. `check_syslog_permissions` runs next and always passes for the virtual root. Then, by action: `READ`, `READ_ALL` and `READ_CLEAR` give `EINVAL` for `bufp == NULL` or `len < 0`, return 0 for `len == 0`, and give `EFAULT` when `access_ok(bufp, len)` fails (a range outside the user address space); `CONSOLE_LEVEL` gives `EINVAL` unless `len` is 1 to 8; any other unknown `type` gives `EINVAL`. With the log always empty no bytes are copied, so an unmapped buffer inside the user address range is not an `EFAULT`. Today `deterministic_syslog_result` (`detcore/src/syscalls/misc.rs:172-179`) takes `len` as a `usize` and never sees `bufp`, because `handle_syslog` passes only `call.priority()` and `call.len()` (`detcore/src/syscalls/misc.rs:436`).

### Differences from Linux

- **`SYSLOG_ACTION_READ` never waits (F2).** On an empty log Linux sleeps until a message arrives; Hermit returns 0 at once. A faithful empty log would sleep forever, which no deterministic run can wake. A klogd-style loop that calls `READ` repeatedly spins under Hermit instead of sleeping. Open question below.
- **No argument checks on the read actions (F2, F3, F4).** `bufp == NULL` or `len < 0` returns 0 under Hermit and `EINVAL` on Linux; an inaccessible buffer returns 0 instead of `EFAULT`. The function never looks at `bufp` or at `len` outside action 8 (`detcore/src/syscalls/misc.rs:176`). This is cheap to fix inside `emulate`.
- **`SYSLOG_ACTION_SIZE_BUFFER` returns 0 (F9).** A real kernel never reports a zero-sized buffer. It is consistent with an empty log, but a program that sizes its read buffer from this value gets a zero-length buffer. Open question below.
- **`len` is read as 64 bits (F7).** The kernel uses the low 32 bits, so `len = 0x100000001` sets level 1 on Linux; Hermit's `len` is a `usize` (`detcore/src/syscalls/misc.rs:172`) and returns `EINVAL`. Only a raw syscall with garbage in the upper half can tell the two apart.
- **Console state is not kept (F6, F7).** The control actions are inert, so the host's console level and `/proc/sys/kernel/printk` never change. No `syslog` action reads that state back.
- **Other kernel-log interfaces are not covered.** `detcore/src/` has no reference to `kmsg` at this SHA, so `/dev/kmsg` and `/proc/kmsg` are not virtualized. A guest that can open them under the host's permissions reads the real log, which contradicts the empty `syslog` view.

## Recommended tests

- T1 (covers F9, F10): under `hermit run --verify` on ptrace, `SYSLOG_ACTION_SIZE_BUFFER` returns 0, and actions 11 and -1 return `EINVAL`, identically across runs.
  Existing partial coverage: `tests/c/syslog_deterministic.c:19` calls `SYSLOG_ACTION_SIZE_BUFFER` and requires a non-negative result; `tests/c/syslog_deterministic.c:26` requires `EINVAL` for action 11. It prints the size, so verify compares it across runs, but the program does not assert 0 and does not try a negative action. Cell `c-programs/syslog-deterministic` (`tests/e2e/manifests/c-programs.yaml:9596`) runs verify on ptrace in CI (`tests/e2e/manifests/c-programs.yaml:9615`).
- T2 (covers F1, F5, F6, F7): `SYSLOG_ACTION_OPEN`, `CLOSE`, `CLEAR`, `CONSOLE_OFF` and `CONSOLE_ON` return 0. `CONSOLE_LEVEL` returns 0 for `len` 1 and 8 and `EINVAL` for 0 and 9; `len = 0x100000001` returns 0, because the kernel truncates `len` to `int` (owner decision 6; Hermit returns `EINVAL` today). From the test harness, check that the host's `/proc/sys/kernel/printk` and the host log are unchanged after the run.
  The unit test at `detcore/src/syscalls/misc.rs:1106` checks these values on the function alone, without a guest.
- T3 (covers F3, F4, F8): `SYSLOG_ACTION_READ_ALL` and `READ_CLEAR` with a valid 4096-byte buffer filled with a marker byte return 0 and leave the buffer untouched; `SIZE_UNREAD` returns 0. Repeat under record and replay and assert the same results.
- T4 (covers F2): `SYSLOG_ACTION_READ` with `len == 0` returns 0. With a valid buffer and `len > 0`, record what Hermit returns (0 today, where Linux would sleep) and assert the contract the owner chooses.
- T5 (covers F2, F3, F4): argument checks for the three read actions: `bufp == NULL` with `len` 16, and a valid buffer with `len` -1, return `EINVAL`; a buffer range above the user address limit (for example `bufp` = 0xffff800000000000) returns `EFAULT`, while an unmapped buffer below the limit returns 0 on the empty log (`READ_ALL`, `READ_CLEAR`), because the kernel's `access_ok` checks only the range. Today Hermit returns 0 for all of them, so this test fails until the checks are added.
- T6 (covers F11): every valid action returns something other than `EPERM` inside the guest. Record the host's `kernel.dmesg_restrict` with the result, and run on hosts with both values where possible, to show the answer does not follow the host.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/495: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/516 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: merged https://github.com/rrnewton/hermit/issues/516 into this issue (no contradiction: both said NONDETERMINISTIC and NEEDS-HANDLER); target_policy investigate -> emulate (Detcore presents an empty, stateless ring buffer: valid actions return 0, controls are inert, invalid actions EINVAL); linux_determinism stays nondeterministic; 11 flag behaviours and 6 tests; READ not waiting, missing EINVAL/EFAULT argument checks and SIZE_BUFFER 0 listed as differences; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied decision 6 (do_syslog check order: int len, permission, read-action EINVAL/0/access_ok EFAULT, CONSOLE_LEVEL 1..8, default EINVAL; T5 EFAULT case narrowed to access_ok; T2 asserts the int truncation of len); target_policy unchanged; no new F/T ids; no questions removed (READ waiting and SIZE_BUFFER are not answered by decisions 6-15).

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `syslog`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+syslog&body=Discussion+for+the+%60syslog%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsyslog.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60syslog%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: syslog" (https://github.com/rrnewton/hermit/issues/495), legacy variant A1. The duplicate https://github.com/rrnewton/hermit/issues/516 was filed the same day with the same title, also legacy variant A1, and was merged into this body section by section on 2026-10-03. The handler was added under the marker `TODO-HUMAN-REVIEW(PR-857)` (https://github.com/rrnewton/hermit/pull/857).

Comment histories:

- https://github.com/rrnewton/hermit/issues/495: see that issue.
- https://github.com/rrnewton/hermit/issues/516: 0 comments, as recorded by the migration.

### Open questions

- Owner decision: should `SYSLOG_ACTION_READ` on the empty virtual log return 0 at once (today), sleep like a real empty log (which never wakes in a deterministic run unless a signal arrives), or be refused? Returning 0 keeps programs moving but turns a blocking reader into a busy loop.
- Owner decision: should `SYSLOG_ACTION_SIZE_BUFFER` keep returning 0, or report a fixed, plausible size (for example a power of two such as the 2097152 the host reported) while reads still return no data?
- `/dev/kmsg` and `/proc/kmsg` are not virtualized at this SHA, so the empty `syslog` view is not the only view of the kernel log a guest may have. That needs its own tracking under the `open`/`read` path, not this syscall.

### Where the merged text went

- Summaries.
  - https://github.com/rrnewton/hermit/issues/495: "Reads from or controls the kernel log ring buffer (dmesg)."
  - https://github.com/rrnewton/hermit/issues/516: "Reads or controls the kernel message ring buffer through the raw system call." with a manual reference to syslog(2).
  - Both are replaced by the Summary, which keeps the syslog(2) reference.
- Linux notes.
  - From https://github.com/rrnewton/hermit/issues/495: "Not the C library syslog(3). Verified: SYSLOG_ACTION_SIZE_BUFFER returned the host buffer size (see evidence)." Kept in the Summary and in F9.
  - From https://github.com/rrnewton/hermit/issues/516: "The raw interface uses action numbers distinct from the libc syslog API. The safe SIZE_BUFFER query returned 2097152 bytes." Kept in the Summary and F9.
- Determinism and nondeterminism sources.
  - From https://github.com/rrnewton/hermit/issues/495: "Reading returns host kernel-log contents and the buffer size, which are entirely host-state dependent and nonreproducible; most read actions are also privileged (dmesg_restrict)." Kept under Determinism analysis. "Most read actions are privileged" is made exact in F11: with `dmesg_restrict` 0, `READ_ALL` and `SIZE_BUFFER` are open to everyone and `READ`, `READ_CLEAR` and `SIZE_UNREAD` are not.
  - From https://github.com/rrnewton/hermit/issues/516: "Kernel log contents, size, sequence, and permissions are host-global and change asynchronously. Hermit must sanitize, record, or deterministically refuse access." and "Concurrent kernel messages, boot history, dmesg restrictions, capabilities, and buffer configuration affect all outputs." Kept under Nondeterminism sources; the "sanitize" option is the one the code chose.
- Classification: both said **NONDETERMINISTIC**; kept.
- Recommended action: both said **NEEDS-HANDLER**. The handler now exists; this body records it as `emulate`.
- Both filings described `syslog` as routed through the old temporary pass-through block. That was true at `dd60278f` and is no longer true: at hermit f4055eb9cae4 the call is `Determinized` and answered by Detcore.

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/495)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=syslog` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `syslog` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
syslog(10 /* SYSLOG_ACTION_SIZE_BUFFER */) = 2097152
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/516)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=syslog`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `syslog` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
syslog(10 /* SYSLOG_ACTION_SIZE_BUFFER */) = 2097152
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
