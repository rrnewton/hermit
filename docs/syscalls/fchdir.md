---
schema: syscall-pages/v1
syscall: fchdir
nr_x86_64: 81
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1942
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchdir&body=Discussion+for+the+%60fchdir%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchdir.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchdir%60%2C+or+in+what+the+page+says%3F%0A
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

`fchdir(fd)` makes the directory that the open descriptor `fd` refers to the calling process's working directory. Hermit forwards it to the host kernel unchanged, like `chdir` and `getcwd`. The kernel's working directory is the only one; Detcore keeps no virtual working directory.

## Linux semantics

The kernel looks up `fd` in the caller's descriptor table. It accepts any open descriptor, including one opened with `O_PATH`. It requires the object to be a directory and checks search (execute) permission on it at the time of the call, not at open time. On success it points the caller's `fs_struct` working directory at that directory and returns 0; otherwise it returns a negative errno and the working directory is unchanged. Nothing is written to guest memory. The new working directory then decides how every relative path resolves, and what `getcwd` and `/proc/self/cwd` report. The working directory belongs to the `fs_struct`, which threads created with `CLONE_FS` share and which `fork` copies.

### Flags and options

- F1: `fd` is open on a directory, in any open mode including `O_RDONLY` and `O_PATH`: the working directory becomes that directory and the call returns 0
- F2: `fd` is not an open descriptor, or is negative: `EBADF`
- F3: `fd` refers to something that is not a directory (a regular file, pipe, socket, or an `O_PATH` descriptor on a non-directory): `ENOTDIR`
- F4: the caller lacks search permission on the directory, checked against its mode and owner at the time of the call: `EACCES`
- F5: the directory was removed or renamed after `fd` was opened: `fchdir` still succeeds. After a rename, relative paths resolve inside the directory under its new name. After a removal, the directory accepts no new entries (`ENOENT`), and `getcwd` fails with `ENOENT`.
- F6: the directory lies outside the caller's root, for example a descriptor opened before a `chroot`: `fchdir` succeeds, and the raw `getcwd` syscall then prefixes its answer with `(unreachable)` (glibc's `getcwd` wrapper turns that into `ENOENT`)
- F7: a descriptor made by `dup`, `dup2` or `fcntl(F_DUPFD)`, inherited across `fork` or `execve`, or received through `SCM_RIGHTS` behaves exactly like the original: it names the same directory
- F8: the change applies to every task that shares the caller's `fs_struct` (threads created with `CLONE_FS`, which includes pthreads). A child created by `fork`, or by `clone` without `CLONE_FS`, has its own copy, and its `fchdir` does not move its parent.

## Determinism analysis

The result is a function of the caller's descriptor table, the directory's mode and owner, the caller's credentials, and the caller's root. None of these depend on timing. Under Hermit, threads run one at a time, so a thread's `fchdir` and another thread's relative path lookup through a shared `fs_struct` (F8) happen in a fixed order. The descriptor numbers themselves are the guest's own and are allocated deterministically from the guest's earlier calls. The call repeats when the guest-visible filesystem starts in the same state and nothing outside the container changes it.

The 2026-08-07 analysis (kept under Additional information) said forwarding "can bind cwd to a host object that differs across runs". That holds only when the descriptor names a host object outside Hermit's fixed container state: a directory changed by a process outside the container, or a descriptor received from outside. That is the same stable-filesystem condition every path syscall in the pass-through block carries, not a property of `fchdir`.

### Nondeterminism sources

- A directory that a process outside the container renames, removes or `chmod`s between runs or during a run (F4, F5).
- A directory descriptor that the guest inherits from the launcher or receives from outside the container. Hermit does not make external I/O deterministic.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `fchdir` is classified `PassThrough` at `detcore/src/syscall_classification.rs:723`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:719` and `:720` gives the condition: "chdir/fchdir/faccessat2/umask are deterministic process-state transitions or checks given a fixed namespace, credential set, and filesystem image". `chdir` is beside it at `detcore/src/syscall_classification.rs:721`.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).
- Detcore knows the descriptor argument: `get_fd` lists `Syscall::Fchdir` at `detcore/src/syscalls/helpers.rs:851`. The pass-through arm does not consult its descriptor table, and it does not need to. On ptrace the descriptor is the guest's own kernel descriptor, so there is no translation step that could make the guest's `fd` name a different object.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists `Sysno::fchdir` at `detcore/src/syscall_classification.rs:2048` and asserts it is `PassThrough` at `detcore/src/syscall_classification.rs:2094`.

The rendered body carried `target_policy: investigate` and a 2026-08-07 strategy draft: resolve the guest descriptor through a deterministic descriptor table, then update a virtual working directory. This body rejects that draft, for the reasons the `getcwd` tracking body gives for the same draft:

- `chdir` (`detcore/src/syscall_classification.rs:721`) and `getcwd` (`detcore/src/syscall_classification.rs:697`) are pass-through. A virtual working directory for `fchdir` alone would disagree with the kernel's, which every relative path, `openat(AT_FDCWD, ...)` and `/proc/self/cwd` use. Keeping the two in step would need Hermit to own path resolution for every path syscall: a filesystem model Hermit does not have.
- Determinism comes from fixing the inputs (the container's filesystem image, the private `/tmp`, fixed credentials, serialized threads), not from rewriting the answer.
- The draft's other concern, translating guest descriptors to host descriptors on other backends, is outside this issue: tracking is ptrace only, and other backends converge on ptrace semantics.

Record and replay. Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore does not see `fchdir`. `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record captures `fchdir`. Two integration tests constrain it:

- `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) runs `tests/c/record_replay_path_queries.c`. Its raw `syscall(SYS_fchdir, dirfd)` at `tests/c/record_replay_path_queries.c:148` uses a descriptor passed through `SCM_RIGHTS`, and must replay into the recorded directory.
- `record_refuses_a_removed_working_directory` (`hermit-cli/tests/record_replay.rs:2200`) requires record to refuse an `fchdir` into a removed directory (`tests/c/record_replay_path_queries.c:112`). That is a record-mode limitation; in `hermit run` the call succeeds as on Linux (F5).

The open gap is test evidence, not code. The only run-mode test that calls `fchdir` is `tests/c/cwd_roundtrip.c:46`. It uses glibc `fchdir()`, which issues `SYS_fchdir` on this host's glibc 2.34 (host strace under Additional information).

## Recommended tests

- T1 (covers F1, F7): under `hermit run --verify` on ptrace, open a fresh directory with `O_RDONLY` and with `O_PATH`, `dup` each, and call raw `syscall(SYS_fchdir, ...)` through every descriptor. After each call, create a file by relative path, find it by absolute path, and compare `getcwd`. Repeat with a descriptor inherited across `fork` and one received through `SCM_RIGHTS`.
  Existing partial coverage: `tests/c/cwd_roundtrip.c:46` (glibc `fchdir`, back to the start directory; cell `c-programs/cwd-roundtrip` at `tests/e2e/manifests/c-programs.yaml:16268`, verify with ptrace `ci: true`).
- T2 (covers F2, F3): assert `EBADF` for a closed descriptor and for `-1`. Assert `ENOTDIR` for a regular-file descriptor, a pipe, and an `O_PATH` descriptor on a regular file.
- T3 (covers F4): assert `EACCES` for a directory whose search bit the caller lacks. Under ptrace the guest is root in a user namespace, so use a directory owned by an unmapped host uid with mode `0700`, or drop `CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` first. Also check that the permission is evaluated at `fchdir` time: open the directory, remove its search bit, then call `fchdir`.
- T4 (covers F5): `chdir` into a directory, remove it, and `fchdir` back through a saved descriptor. Assert 0, `ENOENT` from a relative `creat`, and `ENOENT` from `getcwd`. Then rename a directory after opening it, `fchdir` through the descriptor, and assert that a relative `creat` lands under the new name.
  `tests/c/record_replay_path_queries.c:109` to `:112` does the removal sequence, but only `hermit record` runs it, and it requires record to refuse.
- T5 (covers F6): open a directory, `chroot` into one of its subdirectories (`chroot` is pass-through, asserted at `detcore/src/syscall_classification.rs:2119`), then `fchdir` through the saved descriptor. Assert 0 and that raw `syscall(SYS_getcwd, ...)` returns text starting with `(unreachable)`.
- T6 (covers F8): in a two-thread program, one thread calls `fchdir` and the other then opens a relative path, which must resolve in the new directory. In a `fork` child, `fchdir` must leave the parent's working directory unchanged.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. fchdir is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1942: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> deterministic and target_policy investigate -> passthrough (the code forwards at detcore/src/lib.rs:2843; chdir and getcwd are pass-through too, so a virtual working directory for fchdir alone would contradict the kernel's); rejected the 2026-08-07 virtual-cwd draft; listed 8 flag behaviours and 6 tests; no coverage token is recorded (the only run-mode caller is glibc fchdir in tests/c/cwd_roundtrip.c), so tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fchdir`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fchdir&body=Discussion+for+the+%60fchdir%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffchdir.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fchdir%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: fchdir" (https://github.com/rrnewton/hermit/issues/1942), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- None for policy. F6 needs `chroot` inside the guest; if the ptrace CI lane cannot grant it, the owner decides whether F6 may be recorded as unreachable.

### 2026-10-03 host strace evidence

On this host (glibc 2.34, x86_64), a scratch probe calling glibc `fchdir()` under `strace -f` showed the raw syscall:

```
fchdir(3)                               = 0
```

### 2026-08-07 strategy draft (superseded)

Determinism analysis as filed: "fchdir changes cwd from an open descriptor. The answer depends on descriptor identity, directory lifetime, permissions, and backend fd translation; forwarding can bind cwd to a host object that differs across runs."

Hermit design as filed: "Resolve the guest fd through the deterministic fd table to a stable filesystem identity, then update virtual cwd. Preserve Linux semantics for renamed or unlinked directories without exposing a backend host fd."

Legacy required tests (2026-08-07), folded into T1 to T6 above:

- Cover valid directory fds, non-directory fds, closed fds, permission failures, and duplicated fds.
- Rename and unlink the directory before fchdir, then test relative resolution and getcwd behavior.
- Check clone/exec inheritance, repeated strict runs, and record/replay.
- Exercise guest-to-host fd translation on every backend claiming support.

Legacy acceptance text (2026-08-07), superseded by the v1 Done criteria and by `target_policy: passthrough`:

Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

fchdir is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
