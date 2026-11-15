---
schema: syscall-pages/v1
syscall: getcwd
nr_x86_64: 79
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/1946
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getcwd&body=Discussion+for+the+%60getcwd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetcwd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getcwd%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 3
---

## Summary

`getcwd(buf, size)` copies the absolute path of the calling process's working directory into `buf`. Hermit forwards the call to the host kernel, and `chdir`/`fchdir` are forwarded as well. The working directory Hermit starts the guest in is treated as an input to the run, like `argv`.

## Linux semantics

The kernel builds the path by walking from the working directory's dentry up to the process root, crossing mount points in the caller's mount namespace. The result is the path as that namespace sees it at the moment of the call:

- If an ancestor directory has been renamed, by any process, the path shows the new name.
- A symlink used in an earlier `chdir` never appears; the path names the directory it resolved to.

The success return value is the path length including the terminating NUL. glibc's `getcwd()` turns that into a pointer and adds `getcwd(NULL, 0)` allocation, which happens in the library, not in the kernel.

The working directory is shared by threads created with `CLONE_FS`, copied at `fork`, and kept across `execve`.

### Flags and options

- F1: `size` at least the path length plus one: the NUL-terminated absolute path is copied and its length, including the NUL, is returned
- F2: `size` smaller than that, including `size == 0`: `ERANGE`, and `buf` is not written
- F3: `buf` not writable while `size` would be large enough: `EFAULT`
- F4: the working directory has been removed (unlinked): `ENOENT`
- F5: the working directory cannot be reached from the process root, for example after a `chroot` to a subdirectory: the path starts with `(unreachable)`
- F6: the path is longer than `PATH_MAX` (4096): `ENAMETOOLONG`
- F7: path spelling: renames of ancestors are reflected at the time of the call, bind mounts appear as the namespace mounts them, and symlinks used to reach the directory are already resolved
- F8: inheritance: threads created with `CLONE_FS` share the working directory, a `fork`ed child starts with a copy, and `execve` keeps it

## Determinism analysis

`linux_determinism: conditional`. The result is a pure function of three things: the working directory's position in the caller's mount namespace, the names of its ancestors, and `size`. Nothing in the call depends on timing.

It repeats when four conditions hold:

- the run starts in the same directory,
- the mount layout is the same,
- no process outside the guest renames or removes an ancestor during the run,
- the guest's own `chdir`/`rename`/`rmdir` calls happen in the same order. Hermit runs threads one at a time, so they do, including for threads that share a working directory through `CLONE_FS`.

### Nondeterminism sources

- The directory the guest is started in. It is chosen by whoever runs Hermit, and its path text often contains a random temporary name.
- Renames or removals of ancestor directories by processes outside the container.
- The mount layout of the host and the container.

## Hermit design

Target policy: `passthrough`. This matches the code:

- `getcwd` is classified `PassThrough` at `detcore/src/syscall_classification.rs:697`, inside the pass-through block that begins at `detcore/src/syscall_classification.rs:690`. Its comment says these calls are "conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions".
- The generic arm at `detcore/src/lib.rs:2843` forwards it.
- `chdir` and `fchdir` are pass-through too (`detcore/src/syscall_classification.rs:721` and `detcore/src/syscall_classification.rs:723`), so the kernel's working directory is the only one that exists.

The 2026-08-07 strategy draft (under Additional information) proposed producing `getcwd` from a virtual working directory that Hermit would keep itself. This body rejects that draft, for these reasons:

- **The answer must agree with the kernel.** The text `getcwd` returns has to name the same directory the kernel uses for every relative path: `openat(AT_FDCWD, ...)`, `stat(".")`, and `/proc/self/cwd`. A virtual working directory would need Hermit to own path resolution for every syscall that takes a path, or the two would drift apart. That is a filesystem model Hermit does not have.
- **Determinism comes from fixing the inputs.** The way to get determinism is to fix the starting directory and the namespace (the private `/tmp` mount and `--workdir`), not to rewrite the answer.
- **Rewriting would hide a real input.** If two runs start in different directories, the guest may legitimately behave differently. Hiding that difference in `getcwd` alone would make the guest's view inconsistent with the kernel's.

Record and replay:

- Code facts show `subscribed_record_replay: false`. Under `--passthru-opt` Detcore does not see `getcwd`.
- The unit test `passthru_opt_leaves_io_buffer_hashing_blind_only_for_getcwd` at `detcore/src/lib.rs:3234` pins this. It records `getcwd` as the one I/O-buffer-hashed syscall, listed at `detcore/src/io_buffers.rs:344`, that `--passthru-opt` cannot reach.
- `hermit record` has its own subscriptions, in a recorder module that the comment at `detcore/src/lib.rs:1078` names. That module is not in the exported tree.
- The integration test `record_path_queries_and_legacy_mutations` at `hermit-cli/tests/record_replay.rs:2171` requires `chdir`/`getcwd` to replay from the recording, including a path longer than the replayer's 512-byte injection buffer.
- `record_refuses_a_removed_working_directory` at `hermit-cli/tests/record_replay.rs:2200` requires record to refuse an `fchdir` into a removed directory.

Known gap. The working directory the guest inherits is not moved into Hermit's namespace. Hermit applies `--workdir` after it sets up its mounts. A working directory inherited from the launcher stays bound to the host directory, even when Hermit mounts a private directory over that same path, for example `/tmp`. So when the guest is launched from inside a path Hermit later mounts over, `getcwd` may return text such as `/tmp/x` that names a different directory from the one relative paths use. T8 checks this.

## Recommended tests

- T1 (covers F1, F7): under `hermit run --verify` on ptrace, make these `chdir` calls, then call raw `syscall(SYS_getcwd, buf, size)` with `size` exactly equal to the path length plus one:
  - to `/`,
  - to a nested directory by an absolute path,
  - to a nested directory by a relative path,
  - to a directory through a symlink.

  Assert:
  - the return value equals `strlen + 1`;
  - no symlink component appears;
  - the bytes are identical across runs.

  Then rename an ancestor from inside the guest and assert that `getcwd` shows the new name.
  Existing partial coverage, record mode only: `tests/c/record_replay_path_queries.c:213`, driven by `hermit-cli/tests/record_replay.rs:2171`.
- T2 (covers F2): `size` of 0, 1, and the path length (one short of the NUL) each return `ERANGE`, and a canary in `buf` is untouched.
  Existing partial coverage: `tests/c/errno_path_identity.c:127` (`size` 1; cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`, verify ci on ptrace) and `tests/c/record_replay_path_queries.c:222` (`size` 2, record mode).
- T3 (covers F3): `buf = (char *)1` and `buf = NULL`, each with `size` 4096, return `EFAULT`.
- T4 (covers F4): `mkdir d; chdir d; rmdir ../d`, then `getcwd` returns `ENOENT` under `hermit run --verify`.
  The record-mode refusal is already covered by `hermit-cli/tests/record_replay.rs:2200`, using `tests/c/record_replay_path_queries.c` `--removed-cwd`. Run mode has no test.
- T5 (covers F5): `chroot` into a subdirectory while the working directory is outside it. The guest has `CAP_SYS_CHROOT` as root in its user namespace. Assert that the returned path starts with `(unreachable)`.
- T6 (covers F6): build a directory chain whose absolute path is over 4096 bytes, using `mkdirat` and `fchdir` one step at a time, and assert `ENAMETOOLONG`. Also assert that a 4000-byte path succeeds.
  The existing record test reaches only about 600 bytes (`hermit-cli/tests/record_replay.rs:2171`).
- T7 (covers F8): `getcwd` in a forked child, after the child calls `execve` on itself, and in a `CLONE_FS` thread after a sibling thread calls `chdir`. The results must match Linux and be identical across runs.
- T8 (covers F1, F7): start Hermit from a fixed directory, with and without `--workdir`, and check that `stat(".")` and `stat(<getcwd result>)` give the same device and inode.
  Repeat with the launch directory under a path Hermit mounts over, if Hermit accepts such a launch. This measures the known gap described under Hermit design.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. getcwd is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1946: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · p3-samples · p3: linux_determinism conditional (the result depends only on the start directory, the namespace and the guest's own chdir order); target_policy investigate -> passthrough (the answer must agree with the kernel's relative-path resolution, so the 2026-08-07 virtual-cwd draft is rejected); 8 flag behaviours and 8 tests; the inherited-cwd namespace gap is listed for T8

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getcwd`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getcwd&body=Discussion+for+the+%60getcwd%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetcwd.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getcwd%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: getcwd" (https://github.com/rrnewton/hermit/issues/1946), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Inherited working directory and the guest namespace

A measurement on 2026-08-10, filed under https://github.com/rrnewton/hermit/issues/2109 (umbrella https://github.com/rrnewton/hermit/issues/2111), found that bash stats every ancestor of its inherited `$PWD` at startup. A guest whose working directory sits under a shared host directory therefore observes changes that other processes make there.

`--workdir` avoids this because Hermit resolves it after its mounts. Running `cd /tmp` before launching makes it worse, because the inherited working directory binds to the host `/tmp` that Hermit's mount hides.

This agent did not re-check those issues; it has no GitHub access. The finding concerns `stat` of the ancestors rather than the `getcwd` text, but T8 covers the part that touches `getcwd`.

### Legacy strategy draft (2026-08-07), rejected above

Determinism analysis: getcwd exposes cwd pathname spelling, mount namespace, rename history, deleted-directory state, buffer sizing, and backend cwd. Host path text can differ even when guest logical state is intended to match.

Hermit design: Render cwd from Hermit's virtual cwd identity and deterministic namespace history. Define Linux-compatible handling for renamed/deleted directories and exact buffer/error semantics without consulting backend cwd text.

Legacy required tests (2026-08-07), now T1 to T8:

- Cover root, nested and relative chdir, renamed ancestors, deleted cwd, mount boundaries, and symlinks.
- Exercise zero/small/exact/large buffers and invalid pointers with exact errno checks.
- Assert byte-identical paths across strict repeats and record/replay.
- Test clone/exec inheritance and every backend claiming support.

The last item's "every backend" part is dropped: tracking is ptrace only.

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

getcwd is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
