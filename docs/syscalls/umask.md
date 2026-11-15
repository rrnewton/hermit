---
schema: syscall-pages/v1
syscall: umask
nr_x86_64: 95
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/502
  - https://github.com/rrnewton/hermit/issues/524
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+umask&body=Discussion+for+the+%60umask%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fumask.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60umask%60%2C+or+in+what+the+page+says%3F%0A
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

`umask(mask)` sets the calling process's file-mode creation mask and returns the previous mask. Hermit forwards it to the host kernel unchanged. Every later value is a function of the guest's own calls, but the first value the guest sees is the mask it inherited from whatever launched Hermit, and no Hermit code fixes it.

## Linux semantics

The kernel stores `mask & 0777` in the caller's `fs_struct` and returns the old value. The call cannot fail and has no errno. The mask is applied when the kernel creates an inode from a caller-supplied mode: the stored mode is `mode & ~umask`, unless the parent directory has a default POSIX ACL, in which case the ACL decides and the umask is ignored. Glibc's `umask()` issues `SYS_umask` (checked 2026-10-03 on glibc 2.34, x86_64, and in the 2026-07-24 traces below). There is no query-only form; since Linux 4.7 a process can read its mask without changing it from the `Umask:` line of `/proc/<pid>/status`.

Manual reference: [umask(2)](https://man7.org/linux/man-pages/man2/umask.2.html).

### Flags and options

- F1: mask argument: only `mask & 0777` is stored; the setuid, setgid, sticky and file-type bits of `mask` are ignored. The call always succeeds.
- F2: return value: the previous mask, also limited to `0777`. The first call in a process returns the mask inherited from its parent across `fork`, which `execve` keeps.
- F3: sharing: the mask lives in `fs_struct`, so threads and processes created with `CLONE_FS` (every pthread) share one mask and see each other's changes; a child created by `fork` gets a private copy, and `unshare(CLONE_FS)` gives the caller a private copy.
- F4: where it applies: `open`/`openat`/`creat` with `O_CREAT` (including `O_TMPFILE`), `mkdir`/`mkdirat`, `mknod`/`mknodat` (so `mkfifo`), and the socket inode created by `bind` on a pathname `AF_UNIX` socket. It does not apply to `symlink` (links are always `0777`), `link`, `chmod`/`fchmod` or `rename`.
- F5: default ACLs: when the parent directory has a default POSIX ACL, the new inode's mode comes from that ACL masked by the requested mode, and the umask is not applied.
- F6: visibility: the current mask appears in the `Umask:` field of `/proc/<pid>/status` (Linux 4.7 and later).

## Determinism analysis

After the first call, everything is a function of the guest's own call sequence: the stored mask is `mask & 0777`, the return is whatever the guest (or a thread sharing its `fs_struct`) last stored, and the creation modes in F4 follow from it. Under Hermit, guest threads run one at a time, so two `CLONE_FS` threads racing `umask` against `open(O_CREAT)` resolve in a fixed order. The remaining input is environmental: the initial mask is inherited from the process that launched Hermit (`0022` on the host checked, but `0002` and `0077` are common), and it reaches the guest through the first `umask` return (F2), through every file created before the guest sets its own mask (F4), and through `/proc/self/status` (F6). Default ACLs (F5) come from the host filesystem image. So the call is `conditional`: it repeats run to run under the same launcher, as both runs of `--verify` are, but differs between launch environments.

### Nondeterminism sources

- The initial mask, inherited from the launcher's environment (shell, CI runner or service manager). No code in the exported Hermit tree sets it (the only mentions of `umask` in `detcore/src` are the classification at `detcore/src/syscall_classification.rs:719`-`detcore/src/syscall_classification.rs:724`, the string list entry at `detcore/src/syscall_classification.rs:1755` and the test pin at `detcore/src/syscall_classification.rs:2090`), and a search of `hermit-cli/src` at the same SHA found no call either.
- Default POSIX ACLs on directories in the host filesystem image (F5).

### Cross-host variation

- Initial umask of the launch environment (shell, CI runner or service manager): Hermit does not set it, so it reaches the guest through the first `umask` return (F2), through the modes of files created before the guest sets its own mask (F4) and through the `Umask:` line of `/proc/self/status` (F6). It is stable run to run under one launcher and differs between hosts.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `umask` is classified `PassThrough` at `detcore/src/syscall_classification.rs:724`, under the comment at `detcore/src/syscall_classification.rs:719` that "chdir/fchdir/faccessat2/umask are deterministic process-state transitions or checks given a fixed namespace, credential set, and filesystem image", in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it, and the unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins it as `PassThrough` at `detcore/src/syscall_classification.rs:2090`. Code facts show `subscribed_record_replay: false`; `hermit record` uses its own subscriptions in a recorder module (comment at `detcore/src/lib.rs:1078`) that is not in the exported tree.

Forwarding is right for the call itself: the mask is kernel state that the kernel applies inside every creating syscall, so emulating `umask` in Detcore would also require rewriting the mode of every `open`, `mkdir`, `mknod` and `bind`, and reading it back through `/proc`. The classification comment's condition ("given a fixed namespace, credential set, and filesystem image") does not mention the inherited mask, and that is the gap. Closing it belongs in the launcher, not in this syscall's handler: setting a fixed mask before the guest's first instruction would make F2, F4 and F6 independent of the launch environment while keeping `umask` pass-through. Whether to do that is an owner decision (Open questions); until then, a guest that wants a stable result must set its own mask first, as `tests/c/umask_mode.c:56` does, and the comment at `tests/c/umask_mode.c:6`-`tests/c/umask_mode.c:8` says it does so to be "independent of the host's inherited umask".

Existing run-mode evidence, through glibc `umask()`: the cell `c-programs/umask-mode` (`tests/e2e/manifests/c-programs.yaml:14157`, verify mode with `ci` true for ptrace and `bitwise_parity: true`) runs `tests/c/umask_mode.c`, which checks a `0666` file under mask `022` is `0644` (`tests/c/umask_mode.c:59`-`tests/c/umask_mode.c:62`), that `umask(077)` returns `022` (`tests/c/umask_mode.c:68`), that a `0666` file is then `0600` (`tests/c/umask_mode.c:74`-`tests/c/umask_mode.c:77`), that `mkdir(0777)` gives `0700` (`tests/c/umask_mode.c:83`), and that `umask(022)` returns `077` (`tests/c/umask_mode.c:88`). It fails if any of the five steps fails (`tests/c/umask_mode.c:104`). That covers part of F2 and F4. It discards the inherited value, so it cannot show the environmental input, and it calls glibc rather than `syscall(SYS_umask, ...)`, so no coverage token is recorded.

## Recommended tests

- T1 (covers F1, F2): under `hermit run --verify` on ptrace, call raw `syscall(SYS_umask, 07777)`, `syscall(SYS_umask, 0)` and `syscall(SYS_umask, 0x1ff | 0170000)`, and assert each return is the previous value limited to `0777` (`0777`, `0`, then `0777`). Existing partial coverage: `tests/c/umask_mode.c:68` and `tests/c/umask_mode.c:88` (cell `c-programs/umask-mode`).
- T2 (covers F2): print the first `syscall(SYS_umask, 022)` return, run the guest under two launchers with different masks (`sh -c 'umask 002; ...'` and `sh -c 'umask 077; ...'`), and assert that each run returns the launcher's mask: the initial mask is a cross-host input that Hermit does not pin (owner decision 2, 2026-10-04). Then `fork`, `execve` a helper, and assert the child's first return equals the parent's last set value.
- T3 (covers F3): set the mask in one pthread and read it in another (shared); set it in a `fork` child and check the parent is unchanged (private); after `unshare(CLONE_FS)` in a thread, check a change there is invisible to the other threads.
- T4 (covers F4): under mask `027`, create with `open(O_CREAT, 0666)`, `open(O_TMPFILE, 0666)`, `mkdir(0777)`, `mknod(S_IFIFO | 0666)` and `bind` of a pathname `AF_UNIX` socket, and assert `0640`, `0640`, `0750`, `0640` and `0750` (the socket inode starts from `0777`). Assert `symlink` still gives a link whose `lstat` mode is `0777`, and `chmod(path, 0666)` gives `0666`. Existing partial coverage: `tests/c/umask_mode.c:59`, `tests/c/umask_mode.c:74` and `tests/c/umask_mode.c:83`.
- T5 (covers F5): set a default ACL on a directory with `setxattr("system.posix_acl_default")`, set mask `077`, create a file with `0666`, and assert the mode follows the ACL, not the mask. The fixture directory must be on a filesystem that supports ACLs; if it does not, the test fails rather than skips.
- T6 (covers F6): after `umask(037)`, read `/proc/self/status` and assert the `Umask:` line reads `0037`, and that it repeats on both `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/502: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/524 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic -> conditional because the initial mask is inherited from the launcher and no Hermit code fixes it; kept target_policy passthrough (classified at detcore/src/syscall_classification.rs:724, forwarded at detcore/src/lib.rs:2843); merged duplicate https://github.com/rrnewton/hermit/issues/524, whose "only the inherited initial mask is environmental" wins over this issue's unsupported claim that the initial mask is deterministic under Hermit; listed 6 flag behaviours and 6 tests; no coverage token recorded; left todo with an open question for the owner: should the launcher pin the guest's initial umask
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (initial umask inherited from the launch environment); no U items; T2 now asserts the inherited mask; resolved the open question on pinning a fixed umask

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `umask`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+umask&body=Discussion+for+the+%60umask%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fumask.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60umask%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: umask" (https://github.com/rrnewton/hermit/issues/502), legacy variant A1. Duplicate https://github.com/rrnewton/hermit/issues/524 was filed the same day with the same title (legacy variant A1) and is merged into this body; its comment history (0 comments) stays on that issue.

### Merge of the duplicate

- Determinism: this issue said the initial inherited mask "is deterministic under hermit's controlled environment"; the duplicate said "Only the inherited initial mask is environmental". The duplicate wins: no code at hermit f4055eb9cae4 sets the mask (see Nondeterminism sources), so the guest sees the launcher's mask. Both said DETERMINISTIC given the initial mask, which this body keeps as the reason the call is `conditional` rather than `nondeterministic`.
- Both legacy bodies recommended **PASS-THRU**, kept here.
- The duplicate's kernel note ("Linux masks the argument to permission bits and umask always succeeds") is F1. This issue's note that the call "affects default permissions of created files" is F4.
- Both said Hermit routed `umask` through an "UNCLASSIFIED temporary-passthrough block". That was true at their filing SHA (`dd60278f` in the duplicate); at f4055eb9cae4 it is an explicit `PassThrough` arm with a reviewed comment (Hermit design).
- Both traces below show the same line; they are kept as filed.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=umask` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. The legacy body also recorded: "Verified: umask(022) returned the prior mask (18 == 0o022) deterministically."

Observed strace line:
```
umask(022)                      = 022
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence from the duplicate (https://github.com/rrnewton/hermit/issues/524)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=umask`. The harness exited 0.

Observed strace line:
```text
umask(022)                      = 022
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
