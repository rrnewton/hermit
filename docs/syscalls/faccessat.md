---
schema: syscall-pages/v1
syscall: faccessat
nr_x86_64: 269
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/346
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+faccessat&body=Discussion+for+the+%60faccessat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffaccessat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60faccessat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 7
---

## Summary

`faccessat(dirfd, pathname, mode)` asks whether the calling process could read, write or execute `pathname`, resolved against the directory descriptor `dirfd`, or whether it exists, using the process's real user and group ids. Hermit forwards it to the host kernel unchanged, as it does `access` and `faccessat2`; the answer repeats when the guest's container filesystem and credentials are fixed.

## Linux semantics

The kernel resolves `pathname` against `dirfd` (`AT_FDCWD` means the working directory; an absolute path ignores the descriptor), following symlinks in every component including the last, and then runs the normal permission check with the real uid and gid substituted for the effective ones. It returns 0 if every requested permission is granted and a negative errno otherwise; it writes nothing and changes nothing. Root (a process with `CAP_DAC_OVERRIDE` in a user namespace that maps the file's owner) passes read and write checks regardless of mode bits, but passes `X_OK` only if some execute bit is set. Write checks also fail on read-only mounts and on executables being run. Errors are checked in path-walk order, so the errno is a function of the descriptor, the path, the filesystem state, the real credentials and the mount flags.

The x86_64 syscall takes three arguments and no flags; a fourth register value is ignored. The glibc prototype `faccessat(dirfd, pathname, mode, flags)` has a flags argument (`AT_EACCESS`, `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH`) that only `faccessat2` (Linux 5.8, https://github.com/rrnewton/hermit/issues/1941) implements in the kernel. Since glibc 2.33 the wrapper issues `SYS_faccessat2` first, even with `flags = 0`, and falls back to `SYS_faccessat` (emulating the flags in user space) only when the kernel returns `ENOSYS`. On the measurement host (Linux 7.1.3, glibc 2.34), glibc `faccessat()` issued `faccessat2` for both `flags = 0` and `AT_EACCESS` (host check under Additional information). So a program reaches this syscall only through a raw `syscall(SYS_faccessat, ...)`, an older libc, or a kernel without `faccessat2`.

Manual reference: Linux man-pages faccessat(2).

### Flags and options

- F1: `mode` values: `F_OK` (existence only), and any combination of `R_OK`, `W_OK` and `X_OK`
- F2: bits outside `F_OK|R_OK|W_OK|X_OK` in `mode`: `EINVAL`
- F3: the check uses the real uid and gid, not the effective ones, and the real uid's capabilities; a real-root process passes `R_OK` and `W_OK` on any file whose owner its namespace maps, and passes `X_OK` only if at least one execute bit is set
- F4: inside a user namespace, root has no override on files owned by an unmapped uid; those are checked against the "other" bits
- F5: `dirfd`: `AT_FDCWD` resolves a relative `pathname` against the working directory, inside the caller's root and mount namespace; any other descriptor must be an open directory. A relative path with a descriptor that is not open: `EBADF`. A relative path with a descriptor that is open but not a directory: `ENOTDIR`. An absolute path ignores the descriptor. An empty `pathname` gives `ENOENT`.
- F6: no flags in the raw syscall: symlinks are followed in every component, including the last, and a fourth register value is ignored. `AT_EACCESS`, `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` exist only in `faccessat2` or in libc's user-space fallback.
- F7: a component does not exist: `ENOENT`; an intermediate component is not a directory: `ENOTDIR`
- F8: a requested permission is denied, or search permission is denied on a directory in the path: `EACCES`
- F9: `W_OK` on a file on a read-only mount: `EROFS`
- F10: `W_OK` on a regular file currently being executed: `ETXTBSY`
- F11: a component longer than `NAME_MAX` or a path longer than `PATH_MAX`: `ENAMETOOLONG`; too many symlinks: `ELOOP`; `pathname` outside the address space: `EFAULT`
- F12: `X_OK` on a file on a `noexec` mount: `EACCES`

## Determinism analysis

`conditional`. The result is a function of the descriptor, the filesystem state along the path, the process's real credentials and capabilities, and the mount flags. The descriptor names a directory the guest opened earlier, so it is guest state. Nothing in the call depends on timing. Under Hermit, threads and processes run one at a time, so a racing rename or chmod by another guest thread lands in a fixed order relative to the query. The guest's identity is fixed: Detcore presents a constant virtual root (`getuid` and friends are virtualized to 0, per the comment at `detcore/src/syscall_classification.rs:638`), and on ptrace the guest runs in a user namespace that maps the real uid to 0, so the kernel's real-uid check agrees with that identity. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it. This matches `access` (https://github.com/rrnewton/hermit/issues/1935).

### Nondeterminism sources

- External metadata or ACL mutation: changes to the filesystem made outside the container.
- User namespace policy: which host uids own the files the guest can see. Files owned by uids the namespace does not map fall under F4, so the answer depends on the host image.
- LSM policy on the host, which can deny an access the mode bits allow.
- Host mount flags behind F9 and F12.
- Running without a user namespace (`--no-namespace`), where the kernel checks the host's real uid while Detcore reports 0; that mode is reduced fidelity (see Hermit design).

### Cross-host variation

- Host mount flags on the paths the guest can see: a read-only mount gives `EROFS` for `W_OK` (F9) and a `noexec` mount gives `EACCES` for `X_OK` (F12). The same guest and image get success on a host whose mounts lack these flags.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `faccessat` is classified `PassThrough` at `detcore/src/syscall_classification.rs:771`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The group comment at `detcore/src/syscall_classification.rs:767` and `:768` says these operations "are repeatable in Hermit's fixed mount namespace and filesystem image", and `:769` leaves the boundary open for human review (`TODO-HUMAN-REVIEW(#683)`).
- `faccessat2` is passed through at `detcore/src/syscall_classification.rs:722`, under the comment at `detcore/src/syscall_classification.rs:719` and `:720` ("deterministic process-state transitions or checks given a fixed namespace, credential set, and filesystem image"), and `access` at `detcore/src/syscall_classification.rs:695`. Treating `faccessat` differently from them would make the answer depend on which wrapper libc chose.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::faccessat` at `detcore/src/syscall_classification.rs:2046` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no pathname or metadata authority. Path queries are forwarded to the kernel, and the container's mount namespace is the state. A model of `faccessat` alone would disagree with the `open` that follows it.
- The credential half is already fixed: the guest's ids are a constant virtual root, and on ptrace the kernel's real uid is that root. Once Detcore models `setuid` and friends (owner decision 4) it keeps the record without enforcement: after a modelled `setuid(1000)`, `getuid` reads 1000 but the kernel still checks the namespace root, so `faccessat` keeps answering as root.
- The descriptor is a guest descriptor that the kernel already holds, and the guest's descriptor table is reproduced by its earlier calls, so no translation is needed.
- Refusing `faccessat` would break older libcs and any program that issues it directly, for no gain in repeatability on ptrace.

Record and replay. Code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `faccessat` out. `hermit record` uses its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places outside the exported tree. The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires path queries to replay from the recording rather than from the replay chroot, whose doc comment at `hermit-cli/tests/record_replay.rs:2166` to `:2169` notes the chroot lacks `/etc/passwd` and `/usr/lib`. Its guest asserts that raw `syscall(SYS_faccessat, AT_FDCWD, "/etc/passwd", R_OK)` returns 0 (`tests/c/record_replay_path_queries.c:198` to `:201`) and that `syscall(SYS_faccessat, AT_FDCWD, "/nonexistent/hermit", F_OK)` gives `ENOENT` (`tests/c/record_replay_path_queries.c:206` to `:209`). The harness `canonical_record_replay_command` (`hermit-cli/tests/record_replay.rs:551`) selects `--backend=ptrace` (`hermit-cli/tests/record_replay.rs:562`).

Existing run-mode evidence, on ptrace:

- The cargo integration test `deterministic_file_metadata_syscalls_verify` (`hermit-cli/tests/syscall_file_metadata.rs:51`) runs `tests/c/syscall_file_metadata.c` under `--backend=ptrace run --strict` and then `--verify`. That guest issues raw `syscall(SYS_faccessat, AT_FDCWD, path, R_OK | W_OK)` on a file it created and requires 0 (`tests/c/syscall_file_metadata.c:150` and `:151`). The test asserts that the trace shows `inbound syscall: faccessat(` (`hermit-cli/tests/syscall_file_metadata.rs:91` to `:99`). Cell `c-programs/syscall-file-metadata` at `tests/e2e/manifests/c-programs.yaml:9275` runs the same guest in verify mode with ptrace in CI. This covers part of F1 with `AT_FDCWD` and an absolute path.
- `tests/c/path_file_ops.c:101` calls glibc `faccessat(AT_FDCWD, template, R_OK, 0)` (cell `c-programs/path-file-ops`, `tests/e2e/manifests/c-programs.yaml:12961`). On glibc 2.33 or later that call issues `faccessat2`, not `faccessat`, so it is not evidence for this syscall.

No coverage token is recorded yet: these tests cover part of F1 and F7, and the remaining behaviours have no test.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, the kernel checks the launching host user's real uid and capabilities while Detcore reports uid 0, so `faccessat` can return `EACCES` on a root-owned `0600` file the guest believes it can read.

## Recommended tests

- T1 (covers F1, F3, F5): under `hermit run --verify` on ptrace, create files with modes `0000`, `0400`, `0600`, `0700` and `0644` in a fresh directory and call raw `syscall(SYS_faccessat, ...)` with `AT_FDCWD` and relative paths, with a real directory descriptor and relative paths, and with absolute paths and an invalid descriptor (which must be ignored), for `F_OK`, each single bit and `R_OK|W_OK|X_OK`. Assert the virtual root passes `R_OK`/`W_OK` on all of them and `X_OK` only on `0700`. Assert `EBADF` for a relative path with descriptor 999, `ENOTDIR` for a relative path with a regular-file descriptor, and `ENOENT` for `""`.
  Existing partial coverage: `tests/c/syscall_file_metadata.c:150` (raw, `AT_FDCWD`, `R_OK|W_OK`), driven by `hermit-cli/tests/syscall_file_metadata.rs:51`.
- T2 (covers F2): assert `EINVAL` for `mode = 0x8`, `mode = 0x10` and `mode = -1`.
- T3 (covers F4, F8): assert `EACCES` for `R_OK` on a `0600` file owned by an unmapped host uid that the guest can see, and for `F_OK` through a directory it cannot search. Record the paths used, which depend on the container image.
- T4 (covers F6, F7): assert a symlink to a `0700` file answers like the target, a dangling symlink gives `ENOENT`, a missing intermediate directory gives `ENOENT`, and a regular file used as a directory gives `ENOTDIR`. Pass `AT_SYMLINK_NOFOLLOW` as a fourth register value on the dangling symlink and assert the raw call still gives `ENOENT`, proving the flag is ignored.
  Existing partial coverage: `tests/c/record_replay_path_queries.c:208` (raw `ENOENT`, record mode).
- T5 (covers none): where the guest can see a read-only mount and a `noexec` mount, assert `EROFS` for `W_OK` and `EACCES` for `X_OK` on a file there, and record the mounts. Those errors come from the host's mount flags (host-config, U1), so this test documents them but does not cover F9 or F12.
- T6 (covers F10): copy a small binary, run it in a child that blocks, assert `W_OK` on the copy gives `ETXTBSY`, then reap the child. The test must state whether the host kernel still reports `ETXTBSY` for this case.
- T7 (covers F11): assert `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, `ELOOP` for a symlink loop, and `EFAULT` for `pathname = (char *)1`.
- T8 (covers F1, F7): record/replay on ptrace: a raw `SYS_faccessat` success and an `ENOENT` failure replay from the recording. Existing coverage: `tests/c/record_replay_path_queries.c:200` and `:208`, driven by `hermit-cli/tests/record_replay.rs:2171`.
- U1 (covers F9, F12; host-config): the host's mount flags decide whether `EROFS` (F9) and the `noexec` `EACCES` (F12) occur, so CI cannot produce them on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:771`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/346#issuecomment-5872967056
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/346: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; faccessat records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/346 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional/passthrough (classified at detcore/src/syscall_classification.rs:771, forwarded at detcore/src/lib.rs:2843, same route as access and faccessat2); listed 12 flag behaviours and 8 tests; noted that glibc >= 2.33 faccessat() issues faccessat2, so only raw callers reach this syscall; related now names access and faccessat2; no coverage token recorded because only part of F1 and F7 is tested, so tracking_status stays todo; open question for the owner on --no-namespace real-uid checks, shared with access
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount flags: read-only and noexec); U1 host-config covers F9 and F12, T5 now covers none; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (host real uid checked under --no-namespace and namespace-less backends); noted that a modelled setuid leaves the kernel checking root; target_policy unchanged (passthrough); no new F/T ids; removed the --no-namespace F3 open question
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T8 covered by hermit-cli/tests/record_replay.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `faccessat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+faccessat&body=Discussion+for+the+%60faccessat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffaccessat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60faccessat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: faccessat" (https://github.com/rrnewton/hermit/issues/346), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/346.

Legacy text replaced in this pass: Summary "Rank 67 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls." (a sweep ordering, not a property of the syscall). Determinism analysis "Like the existing access passthrough, it repeats for fixed credentials, dirfd, and stable filesystem metadata." That claim is kept above.

### Open questions

- F10: whether current host kernels still return `ETXTBSY` for `W_OK` on a running executable is not confirmed here; T6 should establish it before it is required.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=faccessat -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(269, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
faccessat(-1, 0xffffffffffffffff, R_OK|W_OK|X_OK|0xfffffff8) = -1 EINVAL (Invalid argument)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### 2026-10-03 glibc wrapper check

On the measurement host (Linux 7.1.3, glibc 2.34, x86_64), traced with `strace -e trace=faccessat,faccessat2`: glibc `faccessat(dirfd, "b", R_OK, 0)` issued `faccessat2(3, "b", R_OK, 0) = 0`, and with `AT_EACCESS` issued `faccessat2(3, "b", R_OK, AT_EACCESS) = 0`. Raw `syscall(SYS_faccessat, dirfd, "b", 8)` issued `faccessat(3, "b", 0x8 /* ?_OK */) = -1 EINVAL`.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/346#issuecomment-5872967056
