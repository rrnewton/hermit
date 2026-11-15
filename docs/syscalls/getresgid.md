---
schema: syscall-pages/v1
syscall: getresgid
nr_x86_64: 120
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/376
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getresgid&body=Discussion+for+the+%60getresgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetresgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getresgid%60%2C+or+in+what+the+page+says%3F%0A
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

Returns real, effective, and saved group IDs for the calling process.

Rank 97 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls.

Manual reference: Linux man-pages getresgid(2).

Hermit emulates it: Detcore never forwards the call and writes the constant 0 to each result pointer (`detcore/src/syscalls/misc.rs:702-717`), as part of the fixed virtual-root identity. The target is a per-process credential record that the setters update (owner decision 4).

## Linux semantics

The credential subsystem reads the current task credentials and converts kernel GIDs through the active user namespace.

`getresgid(rgid, egid, sgid)` writes the caller's real, effective and saved group IDs to three `gid_t` pointers, in that order, and returns 0. Each ID is converted into the caller's user namespace; an ID with no mapping there is reported as the overflow gid (65534 by default, `/proc/sys/kernel/overflowgid`). The kernel copies the values out one at a time, so a bad pointer fails with `EFAULT` after any earlier values have already been written. A NULL pointer is a bad pointer. The host probe under Additional information shows `EFAULT` for three pointers of `-1`.

The values change only through the credential-setting calls (`setgid`, `setregid`, `setresgid` and so on) and through `execve` of a set-group-ID program.

### Flags and options

- F1: three valid pointers: the real, effective and saved group IDs are written in that order and the call returns 0.
- F2: a NULL or otherwise invalid pointer in any position: `EFAULT`; outputs before the bad one have already been written.
- F3: the reported IDs follow the caller's credential history: `setgid`-family calls and set-group-ID `execve` change them, and IDs unmapped in the caller's user namespace read as the overflow gid.
- F4: every `execve` copies the effective gid into the saved set-group-ID (after any set-group-ID change), so the saved gid read after an `execve` equals the effective gid.

## Determinism analysis

The triplet is fixed by deterministic credential history under the same container credential assumptions as `getgid` and `getegid` (which the legacy text called pass-throughs; at this SHA they are emulated too, see Hermit design).

`linux_determinism: conditional`, kept from the legacy classification. On Linux the result is a pure function of the caller's credential history and of the user namespace it runs in; it repeats when the program is launched by the same user and group with the same namespace mapping, and differs across launching identities or mappings.

### Nondeterminism sources

- External credential mutation through unsupported mechanisms
- User-namespace ID mappings
- The group identity of the launching user, which is the starting point of the credential history

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04). Detcore keeps a per-process credential record: real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets. `getresgid` writes the record's real, effective and saved gids. The record starts with every id 0, `fork` and `clone` copy it, and `execve` keeps it. `setgid`, `setregid`, `setresgid` update it and apply Linux's own permission rules against the record (`CAP_SETGID` from the record's capability sets, which a full uid drop clears); their tracking bodies describe the update side. The record is not enforced: the kernel goes on checking the guest's real credentials, root in its user namespace. The legacy recommendation was **PASS-THRU**; it was superseded by the change tracked in the `TODO-HUMAN-REVIEW(#1549)` comments, which emulates the whole credential-query family (`detcore/src/syscall_classification.rs:657-674`). Those comments record why: pass-through returned 0 under the ptrace backend only because the guest runs in a user namespace that maps the launching identity to 0, while in-process backends had no such namespace and leaked the host gid.

What the code does at hermit f4055eb9cae4, where no record exists yet:

- The dispatch arm calls `handle_getresgid` (`detcore/src/lib.rs:2765`); the comment above the arm states the call is "Never forwarded to the host" (`detcore/src/lib.rs:2752-2759`).
- `handle_getresgid` writes 0 to each of the three pointers that is present and returns 0 (`detcore/src/syscalls/misc.rs:707-716`). The model replaces the three constants with the record's values and keeps the "never forwarded" property.
- The credential setters `setgid`, `setregid`, `setresgid`, `setfsgid` and `setgroups` (and their uid twins) are deterministic no-op successes (`detcore/src/syscall_classification.rs:961-987`, applied at `detcore/src/lib.rs:2272-2276`), so nothing updates the identity today and F3 always reads 0.
- `classify_syscall` returns Determinized for the query family, asserted by the unit test block at `detcore/src/syscall_classification.rs:2308-2326`.
- Reduced fidelity (owner decision 3): under `--no-namespace` (`--core-only`) or a backend without a user namespace, `getresgid` still writes the Detcore values while the kernel checks permissions with the launching host user's credentials, so the triplet does not describe what the kernel allows there. No fidelity is required there.

Why `model` and not the alternatives: `emulate`, the constant 0 of today, makes a privilege drop invisible, so a program that checks its saved gid after dropping sees the drop fail; `passthrough` is the backend-dependent leak that the https://github.com/rrnewton/hermit/pull/1549 change removed; `refuse` would break every program that checks its saved gid before dropping privileges. The accepted cost of `model` is that the modelled ids are not enforced by the kernel.

Known divergences from Linux:

- **NULL pointer.** The handler skips an output whose accessor is `None` (`detcore/src/syscalls/misc.rs:707-715`) and still returns 0. The typed Reverie accessor decodes a NULL address as `None`, so Hermit appears to return 0 where Linux returns `EFAULT`. T2 should confirm this before a defect is filed.
- Target (owner decision 6, 2026-10-04): a NULL output pointer gives `EFAULT`, as on Linux, because the emulated call owes Linux's checks in kernel order. `getresgid` in kernel/sys.c writes the rgid, then the egid, then the sgid with `put_user` and stops at the first fault, so a NULL `egid` returns `EFAULT` with the rgid already written and the sgid untouched. The code change is to treat a `None` accessor at `detcore/src/syscalls/misc.rs:707-715` as `EFAULT` instead of skipping it.
- **Invalid non-NULL pointer.** The memory write's error is returned through `?` (`detcore/src/syscalls/misc.rs:708`); the expected guest-visible errno is `EFAULT`, and earlier outputs stay written, as on Linux. Untested.
- **Credential changes (F3), today.** After a successful `setresgid(1000, 1000, 1000)`, Linux reports 1000; Hermit reports 0. The model removes this difference.
- **`execve` (F4).** Decision 4 says `execve` keeps the record, so the saved gid read after an `execve` is the one from before it. Linux copies the effective gid into the saved set-group-ID on every `execve` and honours the set-group-ID bit (a documented difference).
- **Thread scope (target).** The record is per process; on Linux a raw `setresgid` syscall changes only the calling thread.
- **`/proc/<pid>/status` is not covered.** Its `Gid:` line shows the kernel's credentials, so it disagrees with the record after a change. That stays an open item for `/proc` virtualization.

## Recommended tests

- T1 (covers F1): call `getresgid` with three valid pointers prefilled with `-1` and check all three read 0, the record's starting value; run under `hermit run --verify` on ptrace and under record and replay.
  Existing partial coverage: `deterministic_passthrough_syscalls_verify` (`hermit-cli/tests/syscall_quick_wins.rs:30`) runs `tests/c/syscall_quick_wins.c`, which calls `getresgid` with `-1`-prefilled outputs (`tests/c/syscall_quick_wins.c:30-43`), checks only that each output changed, and prints the values in its success line (`tests/c/syscall_quick_wins.c:145-153`) for `--verify` to compare; the test also checks the strict trace names the call (`hermit-cli/tests/syscall_quick_wins.rs:67-81`). Its manifest cell is `c-programs/syscall-quick-wins` (`tests/e2e/manifests/c-programs.yaml:9345`, verify ci at `tests/e2e/manifests/c-programs.yaml:9362-9367`). Neither asserts the value 0.
- T2 (covers F2): a matrix with each of the three pointers in turn set to NULL and to a `PROT_NONE` page, checking the errno and which earlier outputs were written; compare against Linux natively. A NULL pointer must give `EFAULT` with the earlier outputs written (owner decision 6); today Hermit is expected to return 0, so this part fails until the handler changes.
- T3 (covers F3): read-back through the record under `--strict` on ptrace. As root, `setresgid(1000, 1001, 1002)` returns 0 and `getresgid` reports 1000, 1001, 1002; `setgroups(1, {3000})` leaves the triplet unchanged. After `setresuid(1000, 1000, 1000)` drops every uid, `setresgid(-1, 1002, -1)` (unprivileged, allowed because 1002 is the saved gid) makes it report 1000, 1002, 1002, and `setresgid(0, -1, -1)` fails with `EPERM` and leaves it unchanged. A forked child reports the same triplet.
- T4 (covers F4): after `setresgid(1000, 1001, 1002)`, `execve` an ordinary helper that prints `getresgid`. Linux reports 1000, 1001, 1001. Assert 1000, 1001, 1002, because decision 4 keeps the record across `execve` (a documented difference), identically across runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/376: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy passthrough -> emulate (the code writes the fixed virtual-root 0 and never forwards; legacy PASS-THRU superseded by the https://github.com/rrnewton/hermit/pull/1549 change); linux_determinism stays conditional; 3 flag behaviours and 3 tests listed; NULL-pointer divergence (0 instead of EFAULT) recorded for T2 to confirm; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (per-process credential record, read-back of setresgid/setregid/setgid, Linux permission rules against the record, no enforcement); Hermit design rewritten with a reduced-fidelity line for --no-namespace and namespace-less backends; new F4 (execve copies the effective gid into the saved set-group-ID) and T4; T3 now asserts read-back (1000,1001,1002; unprivileged change to the saved gid after a uid drop; EPERM for 0); T4 asserts execve keeps the record (no saved-gid copy, a documented difference); no questions removed (none existed)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (a NULL output pointer gives EFAULT after the earlier outputs are written, as in the kernel's put_user sequence, instead of being skipped; T2 now asserts it); target_policy stays model; no new F/T ids; no open question removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getresgid`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getresgid&body=Discussion+for+the+%60getresgid%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetresgid.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getresgid%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getresgid" (https://github.com/rrnewton/hermit/issues/376), legacy variant A2.

The legacy text called the triplet a pass-through alongside `getgid` and `getegid`. At hermit f4055eb9cae4 all of those are emulated to 0 (`detcore/src/lib.rs:2760-2765`), so the legacy PASS-THRU recommendation no longer describes the code.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=getresgid -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(120, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
getresgid(0xffffffffffffffff, 0xffffffffffffffff, 0xffffffffffffffff) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.
