---
schema: syscall-pages/v1
syscall: swapoff
nr_x86_64: 168
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/484
  - https://github.com/rrnewton/hermit/issues/485
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+swapoff&body=Discussion+for+the+%60swapoff%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fswapoff.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60swapoff%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_privileged_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`swapoff(path)` disables swapping on a swap file or block device, first moving its pages back into memory. Hermit refuses it: Detcore returns a fixed `EPERM` without forwarding the call or reading the path (`detcore/src/lib.rs:2164`), which is also what Linux returns to any caller without `CAP_SYS_ADMIN` in the initial user namespace.

Manual reference: [swapoff(2)](https://man7.org/linux/man-pages/man2/swapon.2.html).

## Linux semantics

- The kernel checks `CAP_SYS_ADMIN` with `capable()`, in the initial user namespace, before it looks at `path`. An unprivileged caller, including root inside a user namespace, gets `EPERM` whatever the path is. The legacy probes on Linux 6.17 and a 2026-10-03 probe on Linux 7.1.3 all returned `EPERM` for a nonexistent path.
- With the capability, the kernel resolves `path`, finds the matching active swap area, and swaps every page in it back into memory before removing the area from the host's global swap set. This can take a long time and can fail when memory cannot hold the pages.

### Flags and options

- F1: `path` names an active swap area, privileged caller: the area is drained and disabled; `/proc/swaps` no longer lists it.
- F2: `path` exists but is not an active swap area, privileged caller: `EINVAL`.
- F3: path errors, privileged caller only: `ENOENT`, `EFAULT`, `ENAMETOOLONG`, `ELOOP`, `EACCES`.
- F4: the swapped-out pages do not fit in memory, privileged caller: `ENOMEM`, and the area stays active.
- F5: no `CAP_SYS_ADMIN` in the initial user namespace: `EPERM`, before the path is examined.

## Determinism analysis

`linux_determinism: conditional`. For an unprivileged caller the result is always `EPERM`. For a privileged caller it depends on the host's active swap areas, on memory pressure while the pages are swapped back in, and on path resolution in the host filesystem, and success changes host-wide memory behaviour. Program output does not depend on swap, but the call mutates global state shared by every host process.

### Nondeterminism sources

- `CAP_SYS_ADMIN` in the initial user namespace
- The host's active swap configuration
- Host memory pressure, which decides `ENOMEM` and how long the call takes
- Path resolution in the host filesystem

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `swapoff` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it does not read the path, never forwards the call, and record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

Why `refuse`:

- Swap is host-global, and a forwarded privileged call would drain swap for every process on the machine.
- Because the capability check comes first and is made in the initial user namespace, Hermit's `EPERM` matches Linux for every argument and for the identity Hermit presents (virtual uid 0, `detcore/src/syscall_classification.rs:657-668`). Unlike `swapon`, no errno-order difference remains.
- `emulate` (reporting success) would claim an operation the guest's credentials could not perform.

The code already realizes the policy. The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `swapoff` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers F1, F5): under `hermit run --verify` on ptrace, a guest reads `/proc/swaps`, calls `swapoff` on the first listed area if there is one and otherwise on a temporary regular file, and asserts -1 with `EPERM`. `/proc/swaps` must read the same afterwards. An unprivileged native run also returns `EPERM`, so this pins the guest-visible result rather than telling the refusal from forwarding.
- T2 (covers F2, F3, F4): `swapoff` on a regular file that is not swap, on a nonexistent path and on a NULL pointer: all must return `EPERM`, because the refusal comes before every path and memory check. F4 cannot be provoked directly; this test shows that no argument reaches it.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/484: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/485 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed EPERM at detcore/src/lib.rs:2164-2168, never forwarded; matches Linux for every argument because capable() is checked before the path); linux_determinism stays conditional; merged duplicate https://github.com/rrnewton/hermit/issues/485; 5 flag behaviours and 2 tests listed; no coverage token, so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `swapoff`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+swapoff&body=Discussion+for+the+%60swapoff%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fswapoff.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60swapoff%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: swapoff" (https://github.com/rrnewton/hermit/issues/484), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=swapoff`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `swapoff` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
swapoff("/tmp/hermit-no-swap-b5") = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### Merged duplicate https://github.com/rrnewton/hermit/issues/485

Filed 2026-07-24 as "Syscall classification: swapoff" (legacy variant A1); its comment history (0 comments) stays on that issue. Merged 2026-10-03 by syscall-tracking-p3:

- Both issues classified the call CONDITIONAL; that is kept.
- Its recommended action NEEDS-INVESTIGATION, and NEEDS-HANDLER from https://github.com/rrnewton/hermit/issues/484, are both superseded by the refusal arm Detcore already has.
- Its note "also ENOENT/EINVAL for a bad path" is reconciled as privileged-only (F2, F3): for an unprivileged caller the capability check comes first, which both its own probe and the probes above show as `EPERM` for a nonexistent path.
- Its host evidence, on Linux 6.17 x86_64 with glibc, traced with `strace -f -e trace=swapoff`:

```text
swapoff("/nonexistent-swap")    = -1 EPERM (Operation not permitted)
```

### 2026-10-03 host probe (syscall-tracking-p3)

Linux 7.1.3, uid 212630 with an empty effective capability set, raw `syscall(168, ...)` through Python ctypes: `swapoff("/nonexistent-swap") = -1 EPERM`.
