---
schema: syscall-pages/v1
syscall: chdir
nr_x86_64: 80
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1939
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chdir&body=Discussion+for+the+%60chdir%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchdir.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chdir%60%2C+or+in+what+the+page+says%3F%0A
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

`chdir(path)` makes the directory named by `path` the calling process's working directory, the starting point for every later relative path. Hermit forwards it to the host kernel unchanged, as it does `fchdir` and `getcwd`, so the kernel's working directory is the only one there is.

## Linux semantics

The kernel resolves `path` against the caller's working directory (relative path) or root (absolute path), following symlinks in every component, including the last. It checks that the result is a directory the caller may search, then replaces the working directory in the caller's `fs_struct` with a reference to that directory and its mount. The new working directory is the resolved directory, not the spelling: a symlink used to reach it is not kept.

The call returns `0` or a negative errno. The `fs_struct` is shared by threads created with `CLONE_FS` (glibc's `pthread_create` uses it), copied at `fork`, and kept across `execve`. The working directory is held by reference, so later renames and removals of it do not move the process.

### Flags and options

- F1: path resolution: an absolute `path` starts at the process root, a relative one at the current working directory, and symlinks are followed in every component, including the last. The working directory becomes the resolved directory.
- F2: `.` and `..` components: `..` is taken from the physical directory, so `link/..` after a symlink `link` names the symlink target's parent, not the directory holding the link. `..` at the process root stays at the root.
- F3: success returns `0`. The change is seen by every thread that shares the `fs_struct` through `CLONE_FS`; a `fork`ed child gets a copy that later `chdir` calls on either side do not share; `execve` keeps the working directory.
- F4: the last component, or a prefix component, is not a directory (including `file/` with a trailing slash): `ENOTDIR`
- F5: search permission is denied on the target directory or on any directory in the path: `EACCES`
- F6: a component does not exist, or `path` is empty: `ENOENT`
- F7: other path errors: a component or the path is too long, `ENAMETOOLONG`; too many symlinks, `ELOOP`; `path` points outside the address space, `EFAULT`
- F8: after the call, the working directory is held by reference. If it or an ancestor is renamed, by any process, the process stays in it and relative paths keep working there; `getcwd` shows the new name. If it is removed, the process stays in an empty, unlinked directory: creating a file by relative path gives `ENOENT`, and `getcwd` gives `ENOENT`. The reference also keeps its mount busy.
- F9: host-transient failure: `EIO` when the host storage fails while a directory on the path is read.

## Determinism analysis

`linux_determinism: deterministic`. The result is a function of the filesystem state along the path, the caller's working directory and root, its credentials, and the mount layout. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so a `chdir` in one thread and a `rename`, `rmdir` or relative `open` in another, including threads that share a working directory through `CLONE_FS`, happen in a fixed order. The call repeats exactly when the guest-visible filesystem and the starting directory are the same and nothing outside the container changes them. This matches the truncate and mkdir tracking issues, which count filesystem state as an input.

This differs from `getcwd` (https://github.com/rrnewton/hermit/issues/1946), which is `conditional` because its output is path text that exposes the launch directory's name. `chdir` returns only `0` or an errno, so the launch directory reaches it only through which directories exist relative to it, which is filesystem state.

The migrated body carried `conditional` only as a placeholder, because the 2026-08-07 filing stated no class. Its determinism text named "host namespace, rename races, permissions, mounts, and backend process layout". Renames by the guest are ordered by Hermit's scheduler (F8), permissions and mounts are fixed container inputs, and the remaining external dependencies are listed below.

### Nondeterminism sources

- The directory the guest is started in, which a relative `path` resolves against. It is chosen by whoever runs Hermit.
- Renames or removals of directories on the path by processes outside the container.
- The mount layout of the host and the container.
- `EIO` (F9), a host device failure.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `chdir` is classified `PassThrough` at `detcore/src/syscall_classification.rs:721`, inside the pass-through block that begins at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:719` gives the reason: "chdir/fchdir/faccessat2/umask are deterministic process-state transitions or checks given a fixed namespace, credential set, and filesystem image." `fchdir` is classified the same way at `detcore/src/syscall_classification.rs:723`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` hands the call to the record/replay layer through `record_or_replay_preserving_tool_errors`, which keeps a host errno as the guest's result.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `chdir` as `PassThrough` in the list at `detcore/src/syscall_classification.rs:2044`.

The 2026-08-07 strategy draft proposed tracking the working directory "as a deterministic filesystem identity in the logical process state" and routing all relative resolution through it. This body rejects that draft, and target_policy moves from `investigate` to `passthrough`, for the reasons the getcwd tracking issue gives:

- **Every relative path must agree with the kernel.** `openat(AT_FDCWD, ...)`, `stat(".")`, `/proc/self/cwd` and child processes all use the kernel's working directory. A Hermit-side working directory would need Hermit to own path resolution for every syscall that takes a path, which is a filesystem model Hermit does not have.
- **The ordering half already exists.** The draft's alternative, "serialize rename/unlink interactions", is what Hermit's scheduler does for guest threads.
- **The inputs are already fixed.** The starting directory and the namespace (the private `/tmp` mount and `--workdir`) are fixed by the launch, not by rewriting answers.

Record and replay:

- Code facts show `subscribed_record_replay: false`. The unit test `passthru_opt_leaves_unlisted_passthrough_syscalls_unsubscribed` (`detcore/src/lib.rs:3185`) pins that `chdir` is `PassThrough` and stays outside the `--passthru-opt` subscription, so Detcore never sees it there.
- `hermit record` has its own subscriptions, in the recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree.
- The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires `chdir` to replay from the recording when the replay chroot lacks the directories: into host directories (`tests/c/record_replay_path_queries.c:124`), through a symlink's parent (`tests/c/record_replay_path_queries.c:160`), into a resolved path longer than the replayer's 512-byte injection buffer (`tests/c/record_replay_path_queries.c:181`), into `/usr/lib` (`tests/c/record_replay_path_queries.c:212`), and a missing path with `ENOENT` (`tests/c/record_replay_path_queries.c:224`).
- `record_refuses_a_removed_working_directory` (`hermit-cli/tests/record_replay.rs:2200`) enters a directory with `chdir`, removes it, and requires record to refuse the following `fchdir` back into it (`tests/c/record_replay_path_queries.c:110` to `tests/c/record_replay_path_queries.c:112`).

Known gap, from the getcwd tracking issue: a working directory the guest inherits from the launcher is not moved into Hermit's namespace when Hermit mounts a private directory over that path, so a relative `path` in the guest's first `chdir` can resolve against the host directory rather than the mounted one. That issue's tests check it; this body does not repeat them.

The open gap is test evidence. One ptrace verify cell covers F1 and F3 for absolute paths; no verify cell asserts the errno of a failed `chdir`.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call raw `syscall(SYS_chdir, ...)` with an absolute path, a relative path, a symlink to a directory, `.`, `..`, `link/..` after a symlink `link`, and `/..`. After each, assert `getcwd` and that a relative `open` finds a marker file in the expected directory.
  Existing coverage through glibc `chdir()`, which issues `SYS_chdir` on x86_64: into a fresh `mkdtemp` directory at `tests/c/cwd_roundtrip.c:34` and an absolute-path round trip at `tests/c/cwd_roundtrip.c:57`, each checked with `getcwd` (cell `c-programs/cwd-roundtrip`, `tests/e2e/manifests/c-programs.yaml:16268`, verify ci on ptrace).
- T2 (covers F3): a second thread created with `pthread_create` calls `chdir`; the main thread's `getcwd` must show the change. A `fork`ed child calls `chdir`; the parent's must not change. A child that `chdir`s and then `execve`s `/proc/self/exe` with a flag must report the new directory from `getcwd`.
- T3 (covers F4, F5, F6, F7): assert `ENOTDIR` for a regular file and for `file/`; `EACCES` for a directory without search permission after dropping `CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` with `capset` (the guest is root in a user namespace and holds both by default); `ENOENT` for a missing path and for `""`; `ENAMETOOLONG` for a 256-byte component; `ELOOP` for a symlink loop; `EFAULT` for `path = (char *)1`. Assert the exact errno. `EIO` is F9, recorded as U1 rather than tested.
  Existing tests: `tests/c/errno_path_identity.c:95` checks only that a missing path's result is stable across runs; `tests/c/record_replay_path_queries.c:224` asserts `ENOENT` exactly, but runs under record and replay, not `--verify`.
- T4 (covers F8): `chdir` into a scratch directory, rename it through its absolute path, create a file by relative path, and assert that it appears under the new name and that `getcwd` shows the new name. Then remove the directory through its absolute path and assert that `open("x", O_CREAT)` gives `ENOENT` and `getcwd` gives `ENOENT`.
  Existing record coverage: `record_refuses_a_removed_working_directory` (`hermit-cli/tests/record_replay.rs:2200`), described under Hermit design.
- T5 (covers F1, F8): two guest threads with separate working directories (one calls `unshare(CLONE_FS)`). One renames and removes directories while the other `chdir`s into them and opens relative paths. Assert that every return value and the final directory listing are identical across `--verify` runs.
- T6 (covers F1, F2, F6): record and replay the T1 and T3 sequences with `hermit record` on ptrace and assert the same return values on replay.
  Existing record coverage: `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`), described under Hermit design.
- U1 (covers F9; host-transient): `EIO` comes from failing host storage, which CI cannot produce on demand and which other processes share. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F9 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. chdir is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1939: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> deterministic (returns only 0 or an errno fixed by filesystem state, cwd and credentials; Hermit orders guest renames); target_policy investigate -> passthrough (the code forwards at detcore/src/lib.rs:2843, and a Hermit-side cwd would need a filesystem model, as on getcwd); 8 flag behaviours and 6 tests; no coverage token is recorded, F2 and F4 to F8 lack verify tests and EIO needs fault injection, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): F7 narrowed to guest-determined path errors and EIO split out as new F9, host-transient (U1); T3 no longer asks about EIO; the open question on the unreachable EIO is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `chdir`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chdir&body=Discussion+for+the+%60chdir%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchdir.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chdir%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: chdir" (https://github.com/rrnewton/hermit/issues/1939), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Legacy strategy draft (2026-08-07), rejected above

Determinism analysis: chdir mutates the current working directory used by later relative-path syscalls. Host namespace, rename races, permissions, mounts, and backend process layout can make the resulting path state nondeterministic.

Hermit design: Track cwd as a deterministic filesystem identity in the logical process state and route all relative resolution through it. Serialize rename/unlink interactions or record the resolved identity with replay validation.

Legacy required tests (2026-08-07), now T1 to T6:

- Exercise absolute and relative paths, dot/dot-dot, symlinks, permission errors, and missing paths. (T1, T3)
- Verify later open/stat/getcwd calls use the same virtual cwd after rename or unlink. (T4; there is no virtual working directory, so the test checks the kernel's)
- Compare repeated strict runs and record/replay under concurrent directory mutation. (T5, T6)
- Cover clone/exec inheritance and each backend claiming support. (T2; tests run on ptrace)

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

chdir is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
