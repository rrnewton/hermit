---
schema: syscall-pages/v1
syscall: faccessat2
nr_x86_64: 439
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1941
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+faccessat2&body=Discussion+for+the+%60faccessat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffaccessat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60faccessat2%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 6
---

## Summary

`faccessat2(dirfd, pathname, mode, flags)` checks whether the caller may access a file, resolving `pathname` relative to `dirfd`, with in-kernel handling of `AT_EACCESS`, `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH`. Hermit forwards it to the host kernel unchanged. Under the ptrace backend the kernel checks against the guest's uid 0 in its user namespace, which matches the fixed virtual-root identity that Detcore reports from `getuid`.

## Linux semantics

The kernel validates `mode` and `flags`, resolves the path, then calls the same permission check that `open` uses, with `MAY_ACCESS` set. Without `AT_EACCESS` it temporarily switches to the caller's real uid and gid; with `AT_EACCESS` it uses the effective ids. A caller with `CAP_DAC_OVERRIDE` over the inode passes read and write checks whatever the mode bits say, but an execute check on a non-directory still needs at least one execute bit. After the permission check, a write check on a regular file or directory on a read-only mount fails with `EROFS`. The call returns 0 or a negative errno and copies nothing back to the guest. It changes no state; even the access time is not touched. `faccessat` (269) is the older entry without a flags argument; glibc emulates flags for it in userspace.

### Flags and options

- F1: `mode == F_OK` (0): existence and path resolution only
- F2: `mode` any combination of `R_OK` (4), `W_OK` (2) and `X_OK` (1): permission check against the chosen credentials. With `CAP_DAC_OVERRIDE` over the inode, read and write always pass and execute on a non-directory needs at least one execute bit. Without it, the owner, group or other bits decide.
- F3: `mode` bits outside `07`: `EINVAL`
- F4: `flags` without `AT_EACCESS`: the check uses the real uid and gid (and, for a non-root real uid, no capabilities)
- F5: `AT_EACCESS` (0x200): the check uses the effective uid and gid
- F6: `AT_SYMLINK_NOFOLLOW` (0x100): a symlink as the last component is checked itself, not its target. A dangling symlink then gives 0 for `F_OK` instead of `ENOENT`.
- F7: `AT_EMPTY_PATH` (0x1000): an empty `pathname` checks the file `dirfd` refers to, which may be an `O_PATH` descriptor. Without the flag, an empty `pathname` gives `ENOENT`.
- F8: any other `flags` bit: `EINVAL`
- F9: `dirfd`: `AT_FDCWD` resolves a relative path against the working directory and a directory fd resolves against that directory. An absolute path ignores `dirfd`. A relative path with an invalid `dirfd` gives `EBADF`, and with a non-directory `dirfd` gives `ENOTDIR`.
- F10: `W_OK` on a regular file or directory on a read-only mount: `EROFS`; device, FIFO and socket files are exempt. `W_OK` on an immutable file: `EPERM`.
- F11: `X_OK` on a regular file on a `noexec` mount: `EACCES`, even when the mode has execute bits
- F12: path errors: `ENOENT` (missing component, or a dangling symlink when following), `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, `EFAULT`, and `EACCES` when search permission on a prefix directory is denied
- F13: kernels before 5.8 have no `faccessat2` and return `ENOSYS`; glibc then falls back to emulating the flags through `faccessat` and `fstatat`

## Determinism analysis

The answer is a function of four inputs: the filesystem state along the path (existence, type, mode bits, ownership), the caller's credentials, the mount flags (`ro`, `noexec`), and the inode's immutable flag. Nothing depends on timing, and the call writes no state, so it cannot perturb later calls. Under Hermit, guest threads run one at a time, so a guest thread racing a rename or chmod against the check sees a fixed order. The call repeats exactly when the guest-visible filesystem starts in the same state, nothing outside the container changes it, and the credentials are the same on every run.

### Nondeterminism sources

- Changes to the filesystem made outside the container, including mode or ownership changes on host-owned paths the guest can see.
- The host identity behind the guest. Under ptrace with the default user namespace it is uid 0 mapped from one host uid. Files owned by any other host uid are unmapped in that namespace, so `CAP_DAC_OVERRIDE` does not apply to them and their other-bits decide. That is fixed for a given host and image, but differs between hosts.
- Mount flags of host mounts visible in the guest (F10, F11).

### Cross-host variation

- Host mount flags and inode flags on the paths the guest can see: a read-only mount gives `EROFS` and an immutable file gives `EPERM` for `W_OK` (F10); a `noexec` mount gives `EACCES` for `X_OK` (F11).
- Host uid that launches Hermit: the user namespace maps that one uid to the guest's root, so which host files are unmapped, and therefore checked against their other bits rather than passed by `CAP_DAC_OVERRIDE`, differs between hosts (F2, F12).
- Kernel version: kernels before 5.8 return `ENOSYS` (F13).

## Hermit design

Target policy: `passthrough` (changed from `investigate`, which the migration set as a placeholder because the legacy body stated none). The code already does this at hermit f4055eb9cae4. `faccessat2` is classified `PassThrough` at `detcore/src/syscall_classification.rs:722`, with the comment at `detcore/src/syscall_classification.rs:719` and `detcore/src/syscall_classification.rs:720`: these are "deterministic process-state transitions or checks given a fixed namespace, credential set, and filesystem image". The comment at `detcore/src/lib.rs:2838` says `faccessat2` is untyped in the pinned Reverie revision and is routed through the blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843`. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the class at `detcore/src/syscall_classification.rs:2047` and `detcore/src/syscall_classification.rs:2094`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The container's mount namespace and directory tree are the state, and only the kernel can apply F1 to F13 the same way `open` and `execve` will later see them. An emulated answer that disagreed with a later `open` would be worse than either answer alone.
- The credentials match. Detcore reports the guest as uid and gid 0 (`detcore/src/lib.rs:2760` to `detcore/src/lib.rs:2763`), and under ptrace the guest really is uid 0 in its user namespace, so the kernel's check uses the identity the guest believes it has. The guest's own `setuid` family calls are deterministic no-ops (`detcore/src/syscall_classification.rs:636` to `detcore/src/syscall_classification.rs:655`), so guest code cannot make the real and effective ids differ, and F4 and F5 give the same answer. Executing a set-user-ID file is the one remaining way they can differ, and the kernel handles it. Those no-ops are today's code, not the target: under owner decision 4 Detcore will keep a credential record for the `set*id` family without enforcement, so the record can hold real and effective ids that differ (for example after `setresuid(1000, 0, 0)`) while the kernel still checks namespace root. F4 and F5 then both answer as root; that is the accepted no-enforcement cost, and `target_policy` here stays `passthrough`.
- The call is read-only, so passing it through cannot change state that other syscalls depend on.

The 2026-08-07 strategy draft (kept under Additional information) proposed sharing a "deterministic pathname, dirfd, and virtual-credential authority" with other filesystem queries and implementing flag semantics and error precedence in Hermit. The code at this SHA has no such authority: every path query (`access`, `faccessat`, `faccessat2`, `chdir`) is passed through. This body chooses passthrough over the draft because the kernel already implements the flag semantics and error precedence, and the draft's one real concern, credentials, is met under ptrace. The concern returns where the guest is not root in a user namespace: the comment at `detcore/src/syscall_classification.rs:679` to `detcore/src/syscall_classification.rs:681` records that with `--no-namespace` the host identity is not root. There, `faccessat2` answers for the host user while `getuid` reports 0. Owner decision 3 accepts that as reduced fidelity rather than a gap to close.

Record/replay: code facts show `subscribed_record_replay: false`, so with `--passthru-opt` Detcore does not see the call. `hermit record` has its own subscriptions in a recorder module that is not in the exported tree. The integration test `record_path_queries_and_legacy_mutations` at `hermit-cli/tests/record_replay.rs:2171` requires the result to come from the recording: its guest calls `faccessat2(AT_FDCWD, "/usr/bin", X_OK, AT_EACCESS)` and expects 0 (`tests/c/record_replay_path_queries.c:203` to `tests/c/record_replay_path_queries.c:205`), and the test's comment at `hermit-cli/tests/record_replay.rs:2168` says the replay chroot lacks host paths, so a live query would diverge.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, the kernel checks the launching host user's uid and capabilities while Detcore reports uid 0, so `faccessat2` can return `EACCES` on a root-owned `0600` file the guest believes it can read, with or without `AT_EACCESS`.

## Recommended tests

- T1 (covers F1, F2, F4, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_faccessat2, ...)` on guest-created files with modes 0000, 0644 and 0755 and a directory with mode 0000. For `F_OK`, `R_OK`, `W_OK`, `X_OK` and `R_OK|W_OK|X_OK`, each with flags 0 and with `AT_EACCESS`, assert the exact result. As uid 0: read and write pass on 0000, `X_OK` fails on the 0644 file and passes on the 0755 file and on the 0000 directory.
  Existing partial coverage: `tests/c/faccessat2_flags.c:55` (`F_OK`), `tests/c/faccessat2_flags.c:59` (`R_OK|W_OK`) and `tests/c/faccessat2_flags.c:63` (`R_OK` with `AT_EACCESS`); cell `c-programs/faccessat2-flags` at `tests/e2e/manifests/c-programs.yaml:11570`, verify mode with ptrace enabled and `ci: true`.
- T2 (covers F3, F8): assert `EINVAL` for `mode = 8`, for `mode = ~0`, for `flags = 0xFFFF` and for `flags = 0x2`.
  Existing partial coverage: `tests/c/faccessat2_flags.c:76` (`flags = 0xFFFF`).
- T3 (covers F6): with a dangling symlink, assert `ENOENT` for `F_OK` with flags 0 and 0 with `AT_SYMLINK_NOFOLLOW`. With a symlink to a 0644 file, assert `EACCES` for `X_OK` when following and 0 with `AT_SYMLINK_NOFOLLOW`.
- T4 (covers F7): open a regular file with `O_PATH` and assert that `F_OK` on `""` with `AT_EMPTY_PATH` returns 0 and without it returns `ENOENT`.
- T5 (covers F9): assert that a relative path resolves against the working directory after `chdir` (`AT_FDCWD`) and against an open directory fd. Assert that an absolute path with `dirfd = -1` succeeds, a relative path with `dirfd = -1` gives `EBADF`, and a relative path with a regular-file `dirfd` gives `ENOTDIR`.
  Existing partial coverage: `tests/c/faccessat2_flags.c:84` (directory fd).
- T6 (covers none): on a read-only mount the guest can see, assert `EROFS` for `W_OK` on a regular file and 0 for `W_OK` on a character device. On a `noexec` mount, assert `EACCES` for `X_OK` on a 0755 file. Record the chosen mounts in the test. If ptrace CI cannot provide them, the test must say so rather than pass vacuously. These errors come from the host's mount flags (host-config, U1), so this test documents them but does not cover F10 or F11.
- T7 (covers F12): assert `ENOENT` for a missing path, `ENOTDIR` for a regular file used as a prefix, `ENAMETOOLONG` for a 256-byte component, `ELOOP` for a symlink loop, `EFAULT` for `pathname = (char *)1`, and `EACCES` for a file under a 0700 directory owned by a host uid that is unmapped in the guest's namespace.
  Existing partial coverage: `tests/c/faccessat2_flags.c:68` (missing path).
- T8 (covers F2, F5): record and replay a guest whose `faccessat2` results differ between the recording host and the replay chroot; replay must return the recorded results.
  Existing candidate: `record_path_queries_and_legacy_mutations` at `hermit-cli/tests/record_replay.rs:2171`, which runs on ptrace and checks `X_OK` with `AT_EACCESS` (`tests/c/record_replay_path_queries.c:204`).
- T9 (covers F13): on the CI kernel (5.8 or later), assert that `faccessat2` never returns `ENOSYS` under Hermit, so a fixture cannot silently fall back to glibc's emulation.
- U1 (covers F10, F11; host-config): the host's mount flags and inode flags decide whether `EROFS`, the immutable-file `EPERM` (F10) and the `noexec` `EACCES` (F11) occur, so CI cannot produce them on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:722`, `detcore/src/lib.rs:2843`).
- U2 (covers F13; host-config): the host kernel version decides whether `faccessat2` exists; a kernel before 5.8 returns `ENOSYS`, which Hermit forwards unchanged (`detcore/src/syscall_classification.rs:722`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. faccessat2 is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1941: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional (filesystem state, credentials, mount flags); target_policy investigate -> passthrough (forwarded at detcore/src/lib.rs:2843; the kernel's check uses the guest's namespace uid 0, matching the virtual root getuid reports); listed 13 flag behaviours and 9 tests; no coverage token recorded because no test has been checked against this list, so tracking_status stays todo; open question on --no-namespace credentials
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount and inode flags, launching host uid, kernel version); U1 host-config covers F10 and F11, U2 host-config covers F13, T6 now covers none; resolved the open question on unreachable read-only and noexec mounts
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (host uid checked under --no-namespace and namespace-less backends); noted that modelled set*id records can make real and effective ids differ while the kernel still checks root; target_policy unchanged (passthrough); no new F/T ids; removed the --no-namespace credentials open question and its Open questions heading
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T9 covered by tests/c/faccessat2_flags.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `faccessat2`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+faccessat2&body=Discussion+for+the+%60faccessat2%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffaccessat2.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60faccessat2%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: faccessat2" (https://github.com/rrnewton/hermit/issues/1941), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### 2026-08-07 strategy draft (superseded)

Determinism analysis as drafted: faccessat2 combines dirfd-relative resolution, credential-sensitive permission checks, and flags such as AT_EACCESS, AT_SYMLINK_NOFOLLOW, and AT_EMPTY_PATH. Host files, credentials, and race timing directly affect its answer.

Hermit design as drafted: Share the deterministic pathname, dirfd, and virtual-credential authority used by other filesystem queries. Implement flag semantics and error precedence explicitly; record resolved identity and result only as a bounded transitional design.

Reconciliation (2026-10-03): "race timing" does not apply under Hermit, because guest threads run one at a time; the draft's design is not implemented at hermit f4055eb9cae4 and is replaced by `passthrough` for the reasons under Hermit design. The credential concern is kept as an open question.

Legacy required tests (2026-08-07), folded into T1 to T9:

- Cover each supported flag alone and in combinations, invalid flags, AT_FDCWD, real dirfds, and empty paths.
- Exercise success, denial, dangling symlink, missing path, and rename races.
- Assert exact return and errno across strict repeats and record/replay.
- Bracket unsupported host-dependent behavior with refusal and cover all supporting backends.

The per-backend item is dropped: tracking is ptrace only (owner Q3, 2026-10-02).

Legacy acceptance text (2026-08-07):

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

faccessat2 is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
