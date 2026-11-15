---
schema: syscall-pages/v1
syscall: kcmp
nr_x86_64: 312
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/621
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+kcmp&body=Discussion+for+the+%60kcmp%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkcmp.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60kcmp%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_privileged_observation_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`kcmp(pid1, pid2, type, idx1, idx2)` tells whether two processes share a kernel resource, such as an open file description, an address space or a descriptor table. When they do not, it also returns an ordering of the two objects. Hermit refuses every call with a fixed `EPERM` and never enters the host kernel (`detcore/src/lib.rs:2188` at hermit f4055eb9cae4). That includes a process comparing itself with itself, which succeeds on Linux.

Manual reference: Linux man-pages kcmp(2).

## Linux semantics

`pid1` and `pid2` are resolved in the caller's PID namespace. The caller needs `PTRACE_MODE_READ_REALCREDS` access to both processes. `type` picks the resource, and `idx1`/`idx2` are used only by the types that need them.

The result is 0 if the two objects are the same. Otherwise it is 1 or 2, an ordering of the two kernel object pointers after the kernel scrambles them with per-boot random values (`kptr_obfuscate` in `kernel/kcmp.c`). The ordering is consistent within one boot, so callers such as CRIU can sort objects by it. It is not comparable across boots, and it depends on where the kernel allocated each object.

Common userland callers use `KCMP_FILE` only for its equality answer. systemd and Mesa use it to test whether two descriptors share an open file description, and treat a failure as "cannot tell".

The syscall exists only when the kernel is built with `CONFIG_KCMP` (`CONFIG_CHECKPOINT_RESTORE` on kernels before 5.12); otherwise it returns `ENOSYS`.

### Flags and options

- F1: `KCMP_FILE` (0): whether descriptor `idx1` in `pid1` and descriptor `idx2` in `pid2` refer to the same open file description. A closed descriptor gives `EBADF`.
- F2: `KCMP_VM` (1): whether the two processes share an address space (`CLONE_VM`). `idx1` and `idx2` are ignored.
- F3: `KCMP_FILES` (2): whether they share a file descriptor table (`CLONE_FILES`).
- F4: `KCMP_FS` (3): whether they share filesystem information: root, cwd and umask (`CLONE_FS`).
- F5: `KCMP_SIGHAND` (4): whether they share a signal-handler table (`CLONE_SIGHAND`).
- F6: `KCMP_IO` (5): whether they share I/O context (`CLONE_IO`).
- F7: `KCMP_SYSVSEM` (6): whether they share System V semaphore undo state (`CLONE_SYSVSEM`).
- F8: `KCMP_EPOLL_TFD` (7): whether descriptor `idx1` in `pid1` is registered as a target in the epoll instance named by the `struct kcmp_epoll_slot { efd, tfd, toff }` that `idx2` points to, in `pid2`. A bad pointer gives `EFAULT`, an `efd` that is not open gives `EBADF`, an `efd` that is not an epoll descriptor gives `EINVAL`, and a target that is not registered gives `ENOENT`.
- F9: result encoding: 0 for equal; 1 or 2 for unequal, ordered by scrambled kernel pointers. The ordering is only meaningful within one boot.
- F10: target resolution and permission: `ESRCH` if either pid does not exist in the caller's namespace; `EPERM` if the caller lacks `PTRACE_MODE_READ_REALCREDS` access to either process.
- F11: an unknown `type` (`KCMP_TYPES` or higher) gives `EINVAL`.
- F12: a kernel without `CONFIG_KCMP` gives `ENOSYS`.

## Determinism analysis

`linux_determinism: conditional` (changed from the legacy `nondeterministic`). Whether two objects are the same (result 0 against nonzero) depends only on how the processes were created and which descriptors they hold. That repeats when the guest repeats its own actions. Error results for a bad `type`, a closed descriptor or an absent pid also repeat. The ordering of unequal objects (1 against 2) does not repeat: it depends on where the kernel allocated each object and on random per-boot scrambling. Permission results depend on host credentials and LSM policy.

### Nondeterminism sources

- Kernel object addresses and per-boot random scrambling, which decide 1 against 2 for unequal objects (F9)
- Host PIDs and process lifetimes. The pids are not translated by Detcore (doc comment at `detcore/src/syscall_classification.rs:1151`), so without the refusal a guest could probe processes it does not own.
- Host permission policy for `PTRACE_MODE_READ_REALCREDS` (F10)
- Whether the host kernel has `CONFIG_KCMP` (F12)

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `EPERM`, the errno Detcore returns today, returned before any argument is examined. Linux's `kcmp` checks in this order: the two PID lookups give `ESRCH` (F10); the `PTRACE_MODE_READ_REALCREDS` access checks on both tasks give `EPERM` (F10); then the `type` switch gives `EBADF` for a closed descriptor (F1), `EFAULT` or `ENOENT` for the epoll slot (F8) and `EINVAL` for an unknown type (F11). Under decision 6 the fixed `EPERM` comes before all of these, so a valid comparison and a garbage-argument call get the same `EPERM`, as T1 and T5 assert. Owner decision 9 makes `refuse` the long-term target, including for `KCMP_FILE`. The supportable fallback is the caller's path for a kernel without `kcmp`: callers that use it for an optional "same open file description?" check, such as Mesa and systemd, treat failure as "cannot tell" (as the removed open question put it; not re-checked in their sources for this body). One part of this target is not settled by the decisions: a self-comparison is allowed unprivileged on Linux, which is decision 6's stated exception; this body keeps `EPERM` for every call, as the code does, and records the conflict as an open question. Today's code realizes the target as stated, so no code change is needed unless the owner resolves that question differently:

- The dispatch arm at `detcore/src/lib.rs:2188` matches `is_privileged_observation_refused_syscall` (`detcore/src/syscall_classification.rs:1155`, set `{ptrace, kcmp}` at `detcore/src/syscall_classification.rs:1159`) and returns `Err(Error::Errno(Errno::EPERM))` at `detcore/src/lib.rs:2191`. It comes before any host call, so every `type` and argument (F1-F12) gets `EPERM`. Run, `--verify`, record and replay all see the same result.
- The refusal is unconditional: `refused: true` and `refused_only_when_fail_closed: false`. The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1329`), and `kcmp` is in the Determinized list at `detcore/src/syscall_classification.rs:564`.

Why `refuse` now:

- `EPERM` is a real `kcmp` result (F10): Linux gives it to a caller without read access to a target. Callers that use `kcmp` for an optional check already handle failure.
- `passthrough` would leak the scrambled pointer ordering (F9) and accept untranslated pids (doc comment at `detcore/src/syscall_classification.rs:1151`).
- The refusal matches `process_vm_readv`/`process_vm_writev`, which Hermit also refuses with `EPERM` for the same reason: they address another process through host pids (`detcore/src/syscall_classification.rs:1130`, arm at `detcore/src/lib.rs:2172`). The header of `tests/c/kcmp_refusal.c:8` states the same choice.

Before owner decision 9, a deterministic `model` was considered possible, at least for F1; decision 9 settles `kcmp` as `refuse`. Detcore already tracks open file descriptions: each `DetFd` holds an `Arc<Mutex<OpenFileDescription>>` shared by every descriptor referring to the same Linux `struct file` (`detcore/src/fd.rs:83`), and each description has an `OpenFileId` (`detcore/src/fd.rs:88`). For guest-internal pids, `KCMP_FILE` equality could be answered from that state, and unequal pairs could be ordered by a deterministic id. The other types (F2-F8) would need Detcore to track the identity of address spaces, descriptor tables, fs structs, signal tables, I/O contexts, semaphore-undo lists and epoll registrations. This body found no such tracking for those at this SHA.

Known differences from Linux under this policy:

- A self-comparison, or any comparison Linux would permit, returns `EPERM` instead of 0, 1 or 2. `tests/c/kcmp_refusal.c:12` points out that comparing a process with itself succeeds natively.
- Calls that Linux rejects with `EBADF`, `EINVAL`, `ENOENT`, `EFAULT`, `ESRCH` or `ENOSYS` (F1, F8, F10-F12) all get `EPERM`.

## Recommended tests

- T1 (covers F1, F2, F3, F10): a process compares itself with itself using `KCMP_FILE`, `KCMP_VM` and `KCMP_FILES`, and also calls `kcmp(getpid(), 1, KCMP_FILE, 0, 0)`. Every call returns `-1`/`EPERM` under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/kcmp_refusal.c` (cell `c-programs/kcmp-refusal`, `tests/e2e/manifests/c-programs.yaml:12201`, verify `ci: true` with ptrace enabled), `tests/c/kcmp_eperm.c` (cell `c-programs/kcmp-eperm`, `tests/e2e/manifests/c-programs.yaml:2803`, verify ci on ptrace), and `privileged_observation_is_refused_deterministically` at `hermit-cli/tests/privileged_observation.rs:30`, which runs `kcmp_eperm.c` with `--backend=ptrace run --strict --verify`.
- T2 (covers F4, F5, F6, F7): self-comparison with `KCMP_FS`, `KCMP_SIGHAND`, `KCMP_IO` and `KCMP_SYSVSEM`, and the same four between a parent and a `fork` child. Each returns `EPERM`. The existing fixtures do not call these four types.
- T3 (covers F8): `KCMP_EPOLL_TFD` with a valid slot naming a registered target, with an unregistered target, and with a `NULL` slot pointer. All three return `EPERM`, and the guest's epoll set is unchanged: a following `epoll_wait` still reports the registered target.
- T4 (covers F9): over several `--verify` runs, a guest compares two different descriptors with `KCMP_FILE` and prints the result. The output is always `EPERM`, never 1 or 2, so no scrambled pointer ordering reaches the guest.
- T5 (covers F10, F11, F12): an absent pid, `type = 8` (`KCMP_TYPES`) and `type = -1`, and a closed `idx1` all return `EPERM`. This records the deliberate errno difference from Linux's `ESRCH`/`EINVAL`/`EBADF`. The guest reaches the result without the host kernel, so `ENOSYS` from a kernel without `CONFIG_KCMP` cannot appear.
- T6 (covers F1): record and replay a guest that calls `kcmp`. Both runs return `EPERM`. The kcmp cells have `replay` turned off in the manifest today.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/621: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_observation_refused_syscall, refused.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (detcore/src/lib.rs:2188 returns a fixed EPERM for every type without entering the host); linux_determinism nondeterministic -> conditional (equality is a function of guest actions, only the ordering of unequal objects and host permissions vary); 12 flag behaviours and 6 tests listed; existing kcmp cells named as candidates but no coverage token recorded because nothing was run against this list, so tracking_status stays todo; a KCMP_FILE model from Detcore's open-file-description tracking left as an open question
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed EPERM before any argument check, already realized at detcore/src/lib.rs:2188-2191; kernel check order recorded); target_policy stays refuse; no new F/T ids; removed the open question on modelling KCMP_FILE; added an open question on the self-comparison exception and ENOSYS versus EPERM

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `kcmp`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+kcmp&body=Discussion+for+the+%60kcmp%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fkcmp.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60kcmp%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: kcmp" (https://github.com/rrnewton/hermit/issues/621), legacy variant B.

The legacy Summary ("replace its temporary catch-all policy with an explicit Detcore classification") is stale: since the change the code tags `TODO-HUMAN-REVIEW(PR-853)`, `kcmp` has an explicit Determinized refusal (`detcore/src/syscall_classification.rs:564`, `detcore/src/lib.rs:2188`). The unit test `privileged_observation_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2608`) pins the predicate's membership to exactly `ptrace` and `kcmp`.

`c-programs/kcmp-refusal` is also listed for cross-backend parity in `tests/e2e/parity-selection.yaml:106`.

`tests/e2e/manifests/inventory/test-files.json:787` describes `tests/c/kcmp_refusal.c` as "ci=false", but its cell at `tests/e2e/manifests/c-programs.yaml:12201` has verify `ci: true`. The inventory text looks stale; the manifest is what runs.

### Open questions

- Decision 6 exempts calls that Linux allows unprivileged, and a self-comparison is one: Linux never answers it with `EPERM` (`tests/c/kcmp_refusal.c:12`). Under the standing least-common-denominator policy a refusal must look like a real kernel configuration, and the one that refuses every caller is a kernel built without `CONFIG_KCMP`, which gives `ENOSYS` (F12). Should `kcmp` switch from `EPERM` to `ENOSYS`, so that no case needs an exception? `ptrace`, which shares the arm at `detcore/src/lib.rs:2188-2191`, would keep `EPERM`, so the change would need its own arm. Today's `EPERM` stays the target until the owner decides.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
