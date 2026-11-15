---
schema: syscall-pages/v1
syscall: swapon
nr_x86_64: 167
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/486
  - https://github.com/rrnewton/hermit/issues/488
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+swapon&body=Discussion+for+the+%60swapon%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fswapon.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60swapon%60%2C+or+in+what+the+page+says%3F%0A
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

`swapon(path, swapflags)` enables a file or block device as a swap area, optionally with a priority and discard policy. Hermit refuses it: Detcore returns a fixed `EPERM` without forwarding the call or reading its arguments (`detcore/src/lib.rs:2164`).

Manual reference: [swapon(2)](https://man7.org/linux/man-pages/man2/swapon.2.html).

## Linux semantics

- The kernel first rejects flag bits outside `SWAP_FLAGS_VALID` with `EINVAL`, and only then checks `CAP_SYS_ADMIN` with `capable()`, that is, in the initial user namespace. A 2026-10-03 probe on Linux 7.1.3, unprivileged, returned `EINVAL` for `swapon("/nonexistent-swap", 0x7fffffff)` and `EPERM` for the same path with flags 0. Root inside a user namespace never passes the capability check.
- With the capability, the kernel opens `path`, checks the swap signature and header, and adds the area to the host's global swap set, visible in `/proc/swaps` and to every process on the machine.
- The legacy probes on Linux 6.17 returned `EPERM` for nonexistent paths with flags 0, before any mutation.

### Flags and options

- F1: `SWAP_FLAG_PREFER` (0x8000) with a priority in `SWAP_FLAG_PRIO_MASK` (0x7fff): use that priority. Without it the kernel assigns the next lower negative priority, which depends on how many swap areas the host enabled earlier.
- F2: `SWAP_FLAG_DISCARD` (0x10000): discard (TRIM) swap space on the device.
- F3: `SWAP_FLAG_DISCARD_ONCE` (0x20000), used with `SWAP_FLAG_DISCARD`: discard the whole area once, at `swapon` time.
- F4: `SWAP_FLAG_DISCARD_PAGES` (0x40000), used with `SWAP_FLAG_DISCARD`: discard freed swap pages as they are released.
- F5: undefined flag bits: `EINVAL`, checked before the capability.
- F6: no `CAP_SYS_ADMIN` in the initial user namespace: `EPERM`.
- F7: privileged path and state errors: `ENOENT` (missing path), `EINVAL` (no swap signature or a bad header), `EBUSY` (already in use as swap or otherwise), `EPERM` (the `MAX_SWAPFILES` limit is reached), `ENOMEM`.

## Determinism analysis

`linux_determinism: conditional`. For an unprivileged caller the result is fixed by the arguments: `EINVAL` for undefined flag bits, otherwise `EPERM`. For a privileged caller it depends on the host's swap configuration, on the file or device contents, and on the default priority sequence, and success changes host-wide memory behaviour. Both legacy reports note that swap has no effect on computed results; that holds for program output, but a successful call still changes global state that every host process shares.

### Nondeterminism sources

- `CAP_SYS_ADMIN` in the initial user namespace
- The host's active swap areas, which decide `EBUSY`, the `MAX_SWAPFILES` limit and the default priority
- The swap file or device layout and header
- Host memory policy and pressure once the area is active

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the dispatch arm at `detcore/src/lib.rs:2164-2168` returns `Err(Error::Errno(Errno::EPERM))` for every syscall in `is_privileged_admin_refused_syscall` (`detcore/src/syscall_classification.rs:902`), which lists `swapon` among 16 privileged administration calls. The arm runs before the typed match and before any `record_or_replay` call: it reads neither the path nor the flags, never forwards the call, and record, replay and both `--verify` runs compute the same `EPERM`. The refusal applies in every mode, not only when Hermit fails closed. The rationale comments are at `detcore/src/lib.rs:2154-2163` and `detcore/src/syscall_classification.rs:889-901`, tagged `TODO-HUMAN-REVIEW(#722)` (https://github.com/rrnewton/hermit/issues/722).

Why `refuse`:

- Swap is host-global. No guest-local model of it exists, and a forwarded privileged call would change memory behaviour for every process on the machine.
- The capability is checked in the initial user namespace, so `EPERM` is what Linux returns to every process that is root only inside a user namespace, which is the identity Hermit presents (virtual uid 0, `detcore/src/syscall_classification.rs:657-668`).
- `emulate` (reporting success) would claim an operation the guest's credentials could not perform.

Target (owner decision 6): keep the one fixed `EPERM`, returned before any argument check. Linux's order (`SYSCALL_DEFINE2(swapon)` in `mm/swapfile.c`: `swap_flags & ~SWAP_FLAGS_VALID` gives `EINVAL`, then `capable(CAP_SYS_ADMIN)` gives `EPERM`) puts the flag check first, so for undefined flag bits (F5) the target deliberately answers `EPERM` where an unprivileged Linux caller gets `EINVAL`. Linux allows no unprivileged form of `swapon`, so the decision's exception does not apply. A guest sees what a container without `CAP_SYS_ADMIN` sees; `swapon(8)` reports the error. The code already does this (`detcore/src/lib.rs:2164-2168`), so no Hermit change is needed.

The unit test `privileged_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2535`) checks only that `swapon` is classified Determinized and is in the refusal set; it does not execute the syscall, and as a unit test it cannot serve as a coverage path.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F6): under `hermit run --verify` on ptrace, a guest calls `swapon` on a temporary regular file with flags 0, `SWAP_FLAG_PREFER | 5`, `SWAP_FLAG_DISCARD`, `SWAP_FLAG_DISCARD | SWAP_FLAG_DISCARD_ONCE` and `SWAP_FLAG_DISCARD | SWAP_FLAG_DISCARD_PAGES`. Each must return -1 with `EPERM`, and the guest's `/proc/swaps` must read the same before and after.
- T2 (covers F5): `swapon` with an undefined flag bit (0x100000). It must return -1 with `EPERM` (owner decision 6). Linux returns `EINVAL` to an unprivileged caller, so this case also tells the refusal from forwarding.
- T3 (covers F7): `swapon` on a nonexistent path, on a NULL path and on a file with no swap signature, with flags 0: all must return `EPERM`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/486: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_privileged_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/488 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (fixed EPERM at detcore/src/lib.rs:2164-2168, never forwarded); linux_determinism stays conditional; merged duplicate https://github.com/rrnewton/hermit/issues/488; 7 flag behaviours and 3 tests listed; open question whether undefined flag bits should give EINVAL as on Linux; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied: the fixed EPERM before any argument check is the target, so undefined flag bits (F5) get EPERM, not Linux's EINVAL (code already realizes it, detcore/src/lib.rs:2164-2168); target_policy stays refuse; T2 now asserts EPERM; open question on the errno order removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `swapon`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+swapon&body=Discussion+for+the+%60swapon%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fswapon.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60swapon%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: swapon" (https://github.com/rrnewton/hermit/issues/486), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=swapon`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `swapon` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
swapon("/tmp/hermit-no-swap-b5", 0) = -1 EPERM (Operation not permitted)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### Merged duplicate https://github.com/rrnewton/hermit/issues/488

Filed 2026-07-24 as "Syscall classification: swapon" (legacy variant A1); its comment history (0 comments) stays on that issue. Merged 2026-10-03 by syscall-tracking-p3:

- Both issues classified the call CONDITIONAL; that is kept.
- Its recommended action NEEDS-INVESTIGATION, and NEEDS-HANDLER from https://github.com/rrnewton/hermit/issues/486, are both superseded by the refusal arm Detcore already has.
- Its "no effect on results" note is kept, qualified under Determinism analysis.
- Its host evidence, on Linux 6.17 x86_64 with glibc, traced with `strace -f -e trace=swapon`, matches the evidence above:

```text
swapon("/nonexistent-swap", 0)  = -1 EPERM (Operation not permitted)
```

### 2026-10-03 host probe (syscall-tracking-p3)

Linux 7.1.3, uid 212630 with an empty effective capability set, raw `syscall(167, ...)` through Python ctypes:

```text
swapon("/nonexistent-swap", 0x7fffffff) = -1 EINVAL
swapon("/nonexistent-swap", 0)          = -1 EPERM
```
