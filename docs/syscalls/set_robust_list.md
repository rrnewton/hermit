---
schema: syscall-pages/v1
syscall: set_robust_list
nr_x86_64: 273
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1953
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_robust_list&body=Discussion+for+the+%60set_robust_list%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_robust_list.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_robust_list%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "sysno-guard"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`set_robust_list(head, len)` registers the calling thread's robust-futex list, which the kernel walks at thread death to mark held robust mutexes `FUTEX_OWNER_DIED` and wake a waiter. Hermit forwards the call to the kernel as a reviewed pass-through and remembers the accepted head address, so that under precise futex mode its own exit hook can replay the owner-death wake against Detcore's modeled futex waiters.

## Linux semantics

The call stores `head` in the calling task's `robust_list` field and returns 0. The only check is `len == sizeof(struct robust_list_head)` (24 bytes on x86-64); any other length returns `EINVAL`. The pointer is not dereferenced or validated at call time.

When the thread's memory is released (exit, `exit_group`, a fatal signal, or the `de_thread` step of a successful `execve`), the kernel walks the list from user memory. For each entry, and for the `list_op_pending` entry, it looks at the futex word at `entry + futex_offset`. If the low bits equal the dying thread's TID, it sets the word to `(uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED` and, if `FUTEX_WAITERS` was set, wakes one waiter. glibc then reports `EOWNERDEAD` to the thread that takes the mutex. The walk stops after `ROBUST_LIST_LIMIT` (2048) entries, or at the first fault. A new thread starts with no list (glibc registers each thread's list at thread start), and a successful `execve` clears the field.

### Flags and options

- F1: `len == 24`: registers `head` as the calling thread's robust list and returns 0. A later call replaces the earlier head.
- F2: `len != 24`: returns `EINVAL` and leaves any existing registration unchanged.
- F3: NULL or garbage `head`: accepted without validation. NULL makes the exit walk a no-op; an unmapped head makes the walk fault and stop silently.
- F4: Per-thread lifetime: each thread has its own head; a new thread starts unregistered; a successful `execve` walks the old list (through `de_thread`) and then clears the registration; a failed `execve` keeps it.
- F5: Owner death of a held robust mutex: at thread death the kernel marks each word owned by the dying TID `FUTEX_OWNER_DIED`, keeps `FUTEX_WAITERS`, and wakes one waiter when `FUTEX_WAITERS` was set; the next locker sees `EOWNERDEAD`. This includes the `list_op_pending` entry.
- F6: Death paths: plain thread exit, `exit_group` taking sibling threads with it, and a fatal signal each run the walk for every dying thread.
- F7: Walk anomalies: a list longer than 2048 entries, a cyclic list, or a word not owned by the dying TID is left alone or truncated without any error to anyone.

## Determinism analysis

`linux_determinism: conditional`. The call itself is deterministic: its result depends only on `len`. The determinism concern is the exit-time walk it arms.

### Nondeterminism sources

- The ownership test compares futex words with the dying thread's kernel TID, which is deterministic only when TIDs are allocated in a fixed order (a fresh PID namespace with sequentialized thread creation).
- When a thread dies, and so when the walk runs relative to sibling threads, depends on host scheduling unless the guest's threads are serialized.
- Which waiter the owner-death wake releases, and when it runs, depends on the host futex queue unless the waiters are modeled by a deterministic scheduler.

The call repeats when TIDs come from a deterministic namespace and thread death, futex waits and wakes are ordered by a deterministic scheduler.

## Hermit design

Target policy: `passthrough`. Linux owns the registration, its return value and the atomic owner-word transition at death; Detcore observes the accepted head so its own futex model wakes the same waiter.

What the code does at hermit f4055eb9cae4:

- Classification: a reviewed pass-through (`detcore/src/syscall_classification.rs:711`, review note at `detcore/src/syscall_classification.rs:708-709`).
- Dispatch: a dedicated PassThrough arm calls `handle_set_robust_list` (`detcore/src/lib.rs:2832-2837`); the comment at `detcore/src/lib.rs:2826-2831` says it is "still a pass-through — Linux owns the registration and supplies its result". The call stays subscribed under record/replay's pass-through optimization, because "without it the exit hook cannot replay `exit_robust_list()`" (`detcore/src/lib.rs:1154-1158`).
- Handler: the call is forwarded through `record_or_replay` (`detcore/src/syscalls/threads.rs:1053-1055`), and the head is recorded only after the kernel accepted it, because the `?` returns early on an error. NULL is recorded as no list (`detcore/src/syscalls/threads.rs:1059-1060`), a length other than `ROBUST_LIST_HEAD_LEN` is recorded as no list so the 64-bit walk never parses a compat head (`detcore/src/syscalls/threads.rs:1061-1066`), and the head is stored in thread state (`detcore/src/syscalls/threads.rs:1069`; `ROBUST_LIST_HEAD_LEN = 24` at `detcore/src/syscalls/robust_list.rs:80-82`).
- Owner death: the walk runs only when the backend runs the exit-time robust-list replay, threads are sequentialized and the futex mode is precise (`detcore/src/syscalls/threads.rs:1095-1101`). On ptrace, Detcore reads all sibling lists before `exit_group` or a guest-observed fatal signal destroys the address space, and holds the modeled wakes until the physical-exit callback, after Linux has done the atomic owner-word transition (`detcore/src/syscalls/robust_list.rs:43-46`). Signal delivery stages those wakes (`detcore/src/lib.rs:1462-1470`).
- Exec: a failed `execve` restores the previous head (`detcore/src/tool_local.rs:2433-2437`); an exit whose time cannot be accounted for clears the head without releasing staged wakes (`detcore/src/lib.rs:3016-3020`).

Why not model or emulate: the 2026-08-07 strategy draft (kept below under Additional information) asked Detcore to store the head in logical-thread state and do the robust-futex cleanup itself in the modeled exit path. The code does store the head and does wake the modeled waiter in a deterministic exit hook. It leaves the guest-memory word transition to Linux on purpose (`detcore/src/syscalls/robust_list.rs:43-46`), so there is no second writer racing the kernel. The ownership TID is the raw namespaced TID; TID virtualization is tracked separately as `T78538674` at `detcore/src/lib.rs:1492`.

Known gaps (from `detcore/src/syscalls/robust_list.rs:46-49`):

- Fatal signals that terminate an injected blocking syscall do not provide the guest-bearing exit callback, so their staged wakes are not released through the model.
- The `de_thread` path of a successful `execve` is blocked by a separate ptrace exec-lifecycle failure.
- No test covers F2, F3 or F7 under Hermit.

## Recommended tests

- T1 (covers F1, F4): Register a 24-byte head with `SYS_set_robust_list`, read it back with `get_robust_list(0, ...)`, and confirm the head and length match. Existing candidate: `tests/c/robust_list.c` (cell `c-programs/robust-list`, `tests/e2e/manifests/c-programs.yaml:13532`, verify CI at `tests/e2e/manifests/c-programs.yaml:13550`, backends at `tests/e2e/manifests/c-programs.yaml:13551-13554`).
- T2 (covers F2, F3): `set_robust_list(head, 12)` and `set_robust_list(head, 0)` return `EINVAL` and `get_robust_list` still reports the earlier head; `set_robust_list(NULL, 24)` returns 0 and a thread exit afterwards wakes nobody; an unmapped head returns 0 and the thread exits cleanly.
- T3 (covers F5, F6): A thread holds a robust mutex shared with a sibling thread and exits; the waiting thread gets `EOWNERDEAD`, run under `run --strict --verify` on ptrace. Existing candidate: `hermit-cli/tests/robust_futex_owner_death.rs` `#[test]` `robust_futex_owner_death_wakes_the_waiter_at_l2` (line 196, `--backend=ptrace` at line 208), which builds `tests/bin/robust_futex_test.c`.
- T4 (covers F5): Detcore wakes the modeled waiter before Linux finishes the exit. Existing candidate: `hermit-cli/tests/robust_futex_owner_death.rs` `#[test]` `detcore_wakes_the_modeled_waiter_before_linux_finishes_exit` (line 250, ptrace at line 259).
- T5 (covers F6): Owner death through a guest-observed fatal signal and through `exit_group`. Existing candidate: `hermit-cli/tests/robust_futex_owner_death.rs` `#[test]` `ptrace_wakes_after_guest_observed_fatal_signal_and_exit_group_cleanup` (line 327, ptrace at lines 340 and 372).
- T6 (covers F4): The owner thread execs successfully while holding a robust mutex; the waiter in a sibling process sees `EOWNERDEAD`; a failed `execve` keeps the registration (checked with `get_robust_list`). Blocked today by the exec-lifecycle gap above.
- T7 (covers F7): A list of 2049 entries owned by the dying thread: the first 2048 words get `FUTEX_OWNER_DIED` and the last is untouched; a cyclic list terminates; a word owned by another TID is left unchanged. Run under `--verify`.
- T8 (covers F1, F5): Record and replay T3; replay reports the same wake order and the same `EOWNERDEAD` result.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. set_robust_list is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1953: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch sysno-guard.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> passthrough (Linux owns the registration and the owner-word transition; Detcore records the accepted head and, under precise futex mode on ptrace, wakes its modeled waiter at the exit callback); linux_determinism stays conditional; 7 flag behaviours and 8 tests listed; candidates exist for T1, T3, T4, T5 but no coverage token is recorded (the bin-c/robust-futex-test e2e cell has ptrace verify CI off), so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/robust_list.c, T3 covered by hermit-cli/tests/robust_futex_owner_death.rs, T4 covered by hermit-cli/tests/robust_futex_owner_death.rs, T5 covered by hermit-cli/tests/robust_futex_owner_death.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `set_robust_list`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+set_robust_list&body=Discussion+for+the+%60set_robust_list%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fset_robust_list.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60set_robust_list%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: set_robust_list" (https://github.com/rrnewton/hermit/issues/1953), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

The e2e cell `bin-c/robust-futex-test` (`tests/e2e/manifests/bin-c.yaml:62`) runs `tests/bin/robust_futex_test.c`, but its ptrace verify CI is off (`tests/e2e/manifests/bin-c.yaml:80-81`).

### Legacy strategy draft (2026-08-07)

The legacy body's Hermit design read: "Store robust-list head and length in logical-thread state and perform deterministic robust-futex cleanup in the modeled exit path. Translate guest memory explicitly and avoid relying on host thread teardown ordering." Its determinism note read: "set_robust_list registers per-thread memory used by kernel futex cleanup at thread death. Host registration binds cleanup to host TIDs, addresses, exit timing, and backend behavior." Hermit design above explains how the code at f4055eb9cae4 handles each point.

Legacy required tests (2026-08-07), now folded into T1-T8: valid registration, replacement, invalid lengths and pointers, per-thread isolation; normal and abnormal owner termination with OWNER_DIED and wake ordering; clone, futex wait and wake, exec and exit interleavings under deterministic schedules; strict repeats and record/replay, with a host-cleanup leak negative fixture.

### Legacy acceptance text (2026-08-07)

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

set_robust_list is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
