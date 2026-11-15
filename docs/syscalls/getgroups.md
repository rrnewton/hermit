---
schema: syscall-pages/v1
syscall: getgroups
nr_x86_64: 115
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1947
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getgroups&body=Discussion+for+the+%60getgroups%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetgroups.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getgroups%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 1
---

## Summary

`getgroups(size, list)` returns the calling thread's supplementary group IDs. Hermit forwards it to the host kernel, so the guest sees whatever group list the backend's launch path carried in from the host account, translated into the guest's user namespace, while every other credential query reports a fixed virtual root and `setgroups` is a no-op. The target is `model` (owner decision 4): Detcore reports the supplementary group list of a per-process credential record that starts empty and that `setgroups` updates.

Manual reference: [getgroups(2)](https://man7.org/linux/man-pages/man2/getgroups.2.html).

## Linux semantics

The kernel (`SYSCALL_DEFINE2(getgroups, ...)` in `kernel/groups.c`) reads the supplementary group list from the calling thread's credentials. A negative size fails with `EINVAL`. A size of 0 returns the number of groups and writes nothing. A positive size smaller than the number of groups fails with `EINVAL` and writes nothing. Otherwise the kernel copies each group ID to the user buffer one element at a time (`groups_to_user`), translating each kernel gid into the caller's user namespace with `from_kgid_munged`, so a gid with no mapping in that namespace appears as the overflow gid (65534 unless `/proc/sys/kernel/overflowgid` says otherwise); a fault on any element fails with `EFAULT`, possibly after earlier elements were written. The list is kept in the kernel's sorted order (`groups_sort` when it is set), and it need not contain the effective gid. Credentials are per thread; the list is inherited across `fork`, `clone` and `execve` and changes only through `setgroups`, which needs `CAP_SETGID` in the user namespace and is refused in a namespace whose `/proc/<pid>/setgroups` is `deny`.

### Flags and options

- F1: `size == 0`: return the number of supplementary groups; the buffer is not touched (it may be NULL).
- F2: `size >= count`: copy all `count` group IDs and return `count`.
- F3: `0 < size < count`: `EINVAL`; nothing is written.
- F4: `size < 0`: `EINVAL`.
- F5: the buffer is not writable for the needed length: `EFAULT`, possibly after a prefix of the list was written.
- F6: a group with no mapping in the caller's user namespace is reported as the overflow gid (65534 by default).
- F7: entries come in the kernel's sorted order, and the effective gid is not implied to be present.
- F8: the list belongs to the calling thread's credentials, is inherited across `fork`, `clone` and `execve`, and changes only through `setgroups`.

## Determinism analysis

On one host, under one account and one backend launch path, the answer repeats run to run: nothing in the guest can change it, because `setgroups` is a Detcore no-op (`Sysno::setgroups` in `is_credential_identity_noop_syscall` at `detcore/src/syscall_classification.rs:983`), and the kernel's sorted order (F7) is fixed for a fixed list. It does not repeat across hosts or accounts, and it differs between backends. The ptrace backend runs the guest in a user namespace whose `uid_map` is `0 <caller-uid> 1` according to the comment at `detcore/src/syscall_classification.rs:1004`, and the liteinst test comment at `hermit-cli/tests/liteinst_advanced.rs:422` says that the launcher maps the caller's effective gid to 0 and that "other inherited supplementary groups appear as the overflow GID" (F6). So the guest sees one entry per group of the launching account: 0 for the effective gid if the account lists it, and 65534 for each of the others. On the account this body was written from, `id` reports 21 supplementary groups, the primary gid among them, which by that rule would give a guest list of 0 followed by twenty 65534 entries. That test has to branch on the host account's own `getgroups()` to know what `/usr/bin/groups` will print under Hermit (`hermit-cli/tests/liteinst_advanced.rs:427`), which shows the guest answer is host state. The launcher source (`hermit-cli/src`) is not in the export at hermit f4055eb9cae4, so this rests on that comment, not on code read here. An in-process backend has no such namespace: the comment at `detcore/src/syscall_classification.rs:664` says pass-through "leaked the host uid/gid" there, and the same holds for a forwarded `getgroups`, which would show the raw host gids. So the call is `conditional`: repeatable given a fixed host account and launch path, which is exactly the assumption the virtual-root model was introduced to remove for the other credential queries.

### Nondeterminism sources

- The launching account's supplementary groups: their number, and, on a backend without a user namespace, their values.
- The backend and launch path: user namespace or none, the `gid_map`, and whether `setgroups` was denied before the guest started.
- The host's overflow gid setting, which names the value shown for unmapped groups.

### Cross-host variation

- Supplementary groups of the host account that launches Hermit: under the default `hermit run` on ptrace the call is forwarded (`detcore/src/syscall_classification.rs:718`, `detcore/src/lib.rs:2843`), so the count the guest reads, and whether the list starts with 0, follow that account's group list (F1, F2, F6). The values other than 0 read as the overflow gid. The list becomes fixed (empty at start) once the decision 4 model lands.
- Host overflow gid setting (`/proc/sys/kernel/overflowgid`): the value shown for each unmapped group (F6).

## Hermit design

Target policy: `model` (owner decision 4, 2026-10-04; changed from `emulate`). Detcore keeps a per-process credential record: real, effective, saved and filesystem uid and gid, the supplementary group list and the capability sets. `getgroups` reports the record's group list, which starts empty. `fork` and `clone` copy the record and `execve` keeps it. `setgroups` replaces the list and applies Linux's own rules against the record (`CAP_SETGID` from the record's capability sets, which a full uid drop clears); its tracking body describes the update side. The record is not enforced: the kernel's group list, which it uses for permission checks, does not change.

What the code does at hermit f4055eb9cae4. `getgroups` is classified `PassThrough` at `detcore/src/syscall_classification.rs:718`, under the comment at `detcore/src/syscall_classification.rs:714` that "capget/capset/getgroups observe or update kernel credential state that starts from the fixed container identity on each run"; it is listed in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1704`, pinned as `PassThrough` by `representative_policies_stay_in_their_reviewed_sections` at `detcore/src/syscall_classification.rs:2064`, and forwarded by the generic arm at `detcore/src/lib.rs:2843`. The "fixed container identity" premise holds only per host account and per backend (see Determinism analysis), and the rest of the credential code has already moved off it:

- The credential query family is emulated to the constant virtual root. The comment at `detcore/src/syscall_classification.rs:657` (`TODO-HUMAN-REVIEW(#1549)`) explains that `getuid` and its relatives used to return 0 only because the ptrace backend's user namespace maps the caller to 0, and that emulating them "makes the result backend-independent". `getuid`, `geteuid`, `getgid` and `getegid` return `Ok(0)` at `detcore/src/lib.rs:2763`, and `handle_getresgid` writes 0 to each pointer (`detcore/src/syscalls/misc.rs:702`).
- The credential set family, `setgroups` included, is a no-op success (`detcore/src/syscall_classification.rs:961` and `detcore/src/syscall_classification.rs:983`, `TODO-HUMAN-REVIEW(#787)`), and the `chown` family was completed to match the virtual root (`TODO-HUMAN-REVIEW(#1851)`, `detcore/src/syscall_classification.rs:990`).
- Reduced fidelity (owner decision 3): today, on `--no-namespace` (`--core-only`) and on a backend without a user namespace, the forwarded call shows the launching account's raw host gids rather than 0 and overflow gids. Once the model lands Detcore answers in every mode, but there the kernel still checks the launching user's real groups; no fidelity is required.

So `getgroups` is the one credential query that still reports host-derived state, and after a guest calls `setgroups(0, NULL)` (success) it still reads the inherited list. The model:

- serves the list from the record, starting empty, and never forwards the call;
- applies the kernel's size rules to that list: F1 returns its length, F2 copies it, F3 and F4 return `EINVAL` without writing;
- writes through guest memory and returns `EFAULT` on a fault (F5), as `handle_getresgid` does with `write_value` (`detcore/src/syscalls/misc.rs:708`);
- never reports an overflow gid (F6), keeps the list in the sorted order Linux's `setgroups` gives it (F7), and gives every thread of a process, every child and every exec image the record's list (F8).

Not covered by the model: the `Groups:` line of `/proc/<pid>/status` reads the kernel's list. `sanitize_status` (`detcore/src/procfs.rs:1354`) does not rewrite it (its constants run from `State:` to `nonvoluntary_ctxt_switches:` with no `Groups:`), so the two disagree. That stays an open item for `/proc` virtualization.

Why `model` and not the alternatives: `emulate` with a fixed list would ignore `setgroups`, so a daemon that clears its groups and checks the result would see the drop fail; `passthrough` is today's host-account leak; `refuse` would break `id`, `groups` and every privilege-dropping program.

Existing evidence, all pinning today's host-derived answer: `liteinst_strict_verify_virtual_identity_and_time` (`hermit-cli/tests/liteinst_advanced.rs:412`) runs `/usr/bin/groups` under `--strict --verify` and expects `root` or `root nobody` depending on the host account (`hermit-cli/tests/liteinst_advanced.rs:427`); with an empty list it would expect `root` on every host. The KVM smoke test `run_kvm_pipe_pipe2_and_getgroups_round_trip` (`hermit-cli/tests/cli.rs:4445`) asserts `getgroups(0, NULL) == 1` and a single entry 65534 (`hermit-cli/tests/cli.rs:4493`, `hermit-cli/tests/cli.rs:4494`); it covers F1, F2 and F6 on the KVM backend only, returns early without `/dev/kvm`, and must change to expect an empty list (`[]`, count 0) when the model lands. `rr_getgroups` (`hermit-cli/tests/rr_suite.rs:267`) is `#[ignore]` like every rr test (`hermit-cli/tests/rr_suite.rs:233`). `tests/c/session_identity.c:45` records the related question whether Hermit should model supplementary groups at all, which decision 4 answers yes.

## Recommended tests

- T1 (covers F1, F2, F7): under `hermit run --strict --verify` on ptrace, raw `syscall(SYS_getgroups, 0, NULL)` returns 0 at start (the empty starting list). Then `setgroups(3, {30, 10, 20})`; `getgroups(0, NULL)` returns 3, and calls with exactly that size and with that size plus 8 return 10, 20, 30 in that order, with the oversized buffer's tail keeping its canary.
- T2 (covers F3, F4): after `setgroups(2, {10, 20})`, assert `getgroups(1, buf)` returns -1 with `EINVAL` and leaves a canary-filled `buf` unchanged, and `getgroups(-1, buf)` returns `EINVAL`.
- T3 (covers F5): with a non-empty list, assert `getgroups(count, (gid_t *)1)` returns `EFAULT`, and that a buffer ending at a `PROT_NONE` page one element short returns `EFAULT`.
- T4 (covers F6): assert no entry equals the overflow gid read from `/proc/sys/kernel/overflowgid`, at start and after `setgroups`. Print the `Groups:` line of `/proc/self/status` without asserting it: decision 4 does not cover `/proc`.
- T5 (covers F8): after `setgroups(1, {1234})`, assert the list reads 1234 from a non-main thread, in a `fork` child and after `execve`; `setgroups(0, NULL)` makes it read empty again. After `setresuid(1000, 1000, 1000)` drops every uid, `setgroups(1, {5})` fails with `EPERM` and the list is unchanged.
- T6 (covers none): retired. It compared the ptrace answer with every in-process backend; owner decision 3 sets no fidelity requirement for backends without a user namespace, so nothing is left to test. F1, F2 and F6 are covered by T1 and T4.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. getgroups is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1947: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept linux_determinism conditional (repeats per host account and launch path; differs across accounts and between ptrace, which shows unmapped groups as overflow gids, and in-process backends); target_policy investigate -> emulate (fixed list beside the emulated virtual-root uid/gid at detcore/src/lib.rs:2763, since setgroups is already a no-op at detcore/src/syscall_classification.rs:983); tracking_status todo with an open question for the owner: should the fixed list be empty or [0]; 8 flags; 6 tests; no coverage token recorded
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (the launching host account's supplementary groups and the host overflow gid decide the forwarded list); no U items; no open question resolved (the fixed-list question belongs to pending decision 4)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): target_policy emulate -> model (supplementary list from a per-process credential record that starts empty and that setgroups updates, no enforcement); Hermit design rewritten with a reduced-fidelity line for --no-namespace and namespace-less backends, the KVM test hermit-cli/tests/cli.rs:4445 named as needing to expect [] and the liteinst groups expectation as becoming root, /proc Groups: recorded as not covered; T1 to T5 now assert read-back of setgroups (empty start, sorted order, EPERM after a full uid drop), T6 retired to covers none (in-process parity, decision 3); no new F/T ids; open question on the fixed list's contents removed (answered by decision 4: empty); cross_host_variant stays true (today's forwarded list)


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getgroups`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getgroups&body=Discussion+for+the+%60getgroups%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetgroups.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getgroups%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: getgroups" (https://github.com/rrnewton/hermit/issues/1947), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

getgroups is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### Legacy strategy draft (2026-08-07)

Return a fixed ordered supplementary-group vector from virtual credential state. Define deterministic initialization and transitions, and never inherit an unrecorded host group list.

### Legacy required tests (2026-08-07)

- Cover size query, exact and oversized buffers, undersized buffers, zero groups, and invalid pointers.
- Check exact group count, ordering, values, and errno across repeated runs.
- Pair with credential-changing operations and clone/exec inheritance tests.
- Use distinct host group configurations to prove the guest result is isolated and backend-consistent.

### Legacy acceptance text (2026-08-07)

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.
