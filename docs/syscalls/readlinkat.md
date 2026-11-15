---
schema: syscall-pages/v1
syscall: readlinkat
nr_x86_64: 267
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/394
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readlinkat&body=Discussion+for+the+%60readlinkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadlinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readlinkat%60%2C+or+in+what+the+page+says%3F%0A
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

`readlinkat(dirfd, pathname, buf, bufsiz)` copies the target of a symbolic link, resolved relative to a directory descriptor, into `buf` without a terminating NUL, and returns the number of bytes written. Detcore lets the kernel read the link, then rewrites procfs magic-link targets that would expose host identities: namespace links become fixed names, and `/proc/self/fd/N` links to pipes and sockets get virtual inode numbers (`handle_readlinkat` at `detcore/src/syscalls/namespace.rs:502` at hermit f4055eb9cae4). Ordinary symlinks are returned unchanged.

Manual reference: Linux man-pages readlinkat(2).

## Linux semantics

The kernel resolves `pathname` relative to `dirfd` (or the current directory for `AT_FDCWD`, or the root for an absolute path), without following the final component, and copies the link's contents into `buf`. If the contents are longer than `bufsiz`, it copies the first `bufsiz` bytes and returns `bufsiz`; it never writes a NUL. For procfs magic links (`/proc/<pid>/fd/N`, `/proc/<pid>/exe`, `cwd`, `root`, `ns/*`) the "contents" are generated: a path, or a type and inode such as `pipe:[12345]` or `mnt:[4026531841]`.

Host observation recorded in the legacy issue: `readlinkat(AT_FDCWD, "/proc/self/exe", buf, 64)` returned 23 bytes, the harness path.

### Flags and options

- F1: `dirfd`: `AT_FDCWD` resolves a relative path through the current directory; a directory descriptor (including an `O_PATH` one) resolves it from that directory; an absolute path ignores `dirfd`. A relative path with a closed `dirfd` gives `EBADF`, and with a non-directory `dirfd` gives `ENOTDIR`.
- F2: empty `pathname`: if `dirfd` refers to a symlink (opened with `O_PATH | O_NOFOLLOW`), read that link; otherwise `ENOENT`.
- F3: `bufsiz` and truncation: the result is at most `bufsiz` bytes, silently truncated, with no NUL; `bufsiz <= 0` gives `EINVAL`.
- F4: ordinary symlinks: the stored target string, exactly as it was created.
- F5: procfs namespace links (`/proc/<pid>/ns/<name>` and `/proc/<pid>/task/<tid>/ns/<name>`): `<name>:[<inode>]`, where the inode identifies the namespace and is chosen by the host kernel.
- F6: procfs descriptor links (`/proc/<pid>/fd/N`, `/proc/self/fd/N`, `/proc/thread-self/fd/N`, and a `dirfd` on `/proc/self/fd` with name `N`): a path for files and directories, `pipe:[<inode>]` or `socket:[<inode>]` for pipes and sockets, `anon_inode:[<kind>]` for anonymous inodes, and a path ending in ` (deleted)` for unlinked files. Reading another process's links needs ptrace-read access.
- F7: other procfs links (`/proc/<pid>/exe`, `cwd`, `root`, `map_files/*`): the path of the executable, current directory or root.
- F8: errors: `EINVAL` (the final component is not a symlink, or `bufsiz <= 0`), `ENOENT`, `ENOTDIR`, `EACCES` (search permission, or another process's procfs links), `ELOOP`, `ENAMETOOLONG`, `EBADF`, `EFAULT` (bad `pathname` or `buf`). Host `EIO` and `ENOMEM` are F9.
- F9: host-transient failures: `EIO` from failing storage while the link is read, and `ENOMEM` when the kernel cannot allocate memory. Both depend on host state shared with other processes.

## Determinism analysis

`linux_determinism: deterministic -> conditional`. For an ordinary symlink (F4) the result is a pure function of the filesystem: the same link gives the same bytes. The legacy classification stopped there. Procfs magic links do not repeat across runs:

- Namespace links (F5) embed the namespace inode, which differs between hosts and between container launches.
- Pipe and socket links (F6) embed the pipe or socket inode, which the kernel assigns from a host-wide counter and so differs every run.
- Unlinked `O_TMPFILE` files appear as `/<dir>/#<inode> (deleted)` (F6), again a host inode.
- `exe`, `cwd` and `root` (F7) give host paths, which are stable only if the run uses the same paths.

Truncation (F3) and errors (F8) are deterministic given the same link and arguments. So the call is deterministic for ordinary links and conditional overall.

## Hermit design

Current behaviour (hermit 239ca43d3092): Detcore decides more `/proc` link targets itself instead of only rewriting the kernel's answer. A `readlink` of a `/proc` link whose target Detcore owns — the caller's own descriptor links (including `thread-self` spellings) and namespace links — is answered from Detcore's model on every backend without asking the backend (`readlink_decided_link`, `detcore/src/syscalls/namespace.rs:2055`), the path walk counts links and applies the kernel's lookup checks itself, system-directory links are decided by the object the walk reaches, and descriptors belonging to the in-guest runtime are invisible in the guest's numbering. Not in a nested PID namespace or after a guest `chroot`, where `/proc` need not be Detcore's. Ordinary filesystem symlinks and the namespace-identity rewriting described below are unchanged.

Target policy: `passthrough -> model`. The legacy recommendation was pass-through, but the code models the result: the kernel reads the link, and Detcore replaces the parts that carry host identities. At hermit f4055eb9cae4:

- The dispatch sends `Syscall::Readlinkat` to `handle_readlinkat` (`detcore/src/lib.rs:2419`). `readlinkat` is in the Determinized list under `TODO-HUMAN-REVIEW(#877)` (`detcore/src/syscall_classification.rs:582-587`); the tag names https://github.com/rrnewton/hermit/pull/877, "Determinize proc namespace symlink identities". The unit test at `detcore/src/syscall_classification.rs:1979` asserts the classification for `readlink` and `readlinkat`.
- A null `pathname` fails with `EFAULT` from Detcore (`detcore/src/syscalls/namespace.rs:507`). Detcore works out the path the call names: an absolute path or an `AT_FDCWD` path as given, otherwise the path recorded for `dirfd` joined with `pathname` (`detcore/src/syscalls/namespace.rs:508-515`).
- A `/proc/<pid>/fd/N` link of a process other than the caller goes to `finish_other_proc_fd_readlinkat` (`detcore/src/syscalls/namespace.rs:516-520`, `:394-431`), which reads the raw target through a 32-byte scratch buffer when the guest's buffer is shorter, so it can canonicalize before truncating.
- Every other call runs through `record_or_replay` (`detcore/src/syscalls/namespace.rs:447`), so kernel errors (F1, F2, F3, F8) reach the guest unchanged, then through `canonicalize_namespace_readlink_result` (`detcore/src/syscalls/namespace.rs:275-330`):
  - A namespace link becomes a fixed constant per namespace type, for example `mnt:[4026531841]` (`detcore/src/syscalls/namespace.rs:48-60`, matched by `canonical_namespace_target` at `:64-94`).
  - A `/proc/self/fd/N` link to a pipe or socket becomes `pipe:[<virtual inode>]` or `socket:[<virtual inode>]`, using the same virtual inode the stat family reports (`detcore/src/syscalls/namespace.rs:292-319`).
  - Anything else, including ordinary symlinks, files, `exe`, `cwd`, `root`, memfds and anonymous inodes, is returned as the kernel wrote it (`detcore/src/syscalls/namespace.rs:321`).
  - The rewritten target is truncated to `bufsiz`, as Linux would truncate it (`detcore/src/syscalls/namespace.rs:324-329`).

Why `model`: `passthrough` leaks host namespace and pipe inodes, which programs such as `lsof`, `ss` and container tools print. `emulate` would mean Detcore storing every symlink. `refuse` is impossible: `realpath`, shells and runtimes read symlinks through this call.

Gaps against `model`, by reading the code:

- A relative path through `AT_FDCWD` is not resolved through the current directory (`detcore/src/syscalls/namespace.rs:508-509`), unlike `openat` (`detcore/src/syscalls/files.rs:993-997`). After `chdir("/proc/self/ns")`, `readlinkat(AT_FDCWD, "mnt", ...)` is not recognized and would return the host namespace inode. Likewise a path with `.` or `..` components is not recognized as a namespace link, because `canonical_namespace_target` rejects them (`detcore/src/syscalls/namespace.rs:74`).
- `handle_readlink` rewrites a numeric self path `/proc/<own pid>/fd/N` to `/proc/self/fd/N` before the kernel sees it (`detcore/src/syscalls/namespace.rs:461-490`, helper `host_self_proc_fd_alias` at `:145-151`); `handle_readlinkat` has no such rewrite. Whether the kernel resolves the guest's virtual pid in that path correctly without it was not established here.
- An unlinked `O_TMPFILE` descriptor's `/proc/self/fd/N` target contains the host inode (F6); Detcore returns it unchanged. This is a hypothesis from kernel behaviour, not measured.
- `exe`, `cwd` and `root` targets (F7) are host paths, deterministic only when the run uses the same paths.
- `readlinkat` is not in the `--passthru-opt` allow-list in `Detcore::subscriptions` (`detcore/src/lib.rs:1100-1104`), so under that explicit performance opt-in it bypasses Detcore and no canonicalization happens.

## Recommended tests

- T1 (covers F1, F4): create a file and a symlink to it, then `readlinkat` the link through `AT_FDCWD` with a relative name, through a directory descriptor, through an `O_PATH | O_DIRECTORY` descriptor, and with an absolute path and a bogus `dirfd`. Every result must equal the target string, and output must be identical across `hermit run --verify` on ptrace. A relative name with a closed `dirfd` gives `EBADF`, and with a regular-file `dirfd` gives `ENOTDIR`.
  Existing partial candidates: `tests/c/syscall_file_io.c:168-179` (raw `SYS_readlinkat` through `AT_FDCWD`), driven by the ptrace-only test `deterministic_file_io_syscalls_verify` in `hermit-cli/tests/syscall_file_io.rs` and by the cell `c-programs/syscall-file-io` (verify ci on ptrace); `tests/c/symlink_ops.c:90` (through a directory descriptor; cell `c-programs/symlink-ops`, verify ci on ptrace).
- T2 (covers F2): open a symlink with `O_PATH | O_NOFOLLOW` and call `readlinkat(fd, "", buf, n)`; it must return the target. The same call on a regular file's `O_PATH` descriptor gives `ENOENT`.
- T3 (covers F3): read a 40-byte target with `bufsiz` 10, 40 and 100, and check the returned lengths and that no NUL is written past the data; `bufsiz` 0 gives `EINVAL`. Repeat with `/proc/self/fd/N` on a pipe and a buffer shorter than the canonical `pipe:[...]` string.
- T4 (covers F5): `readlinkat(AT_FDCWD, "/proc/self/ns/<name>")` for every namespace type, through `/proc/self/task/<tid>/ns/<name>`, through a `dirfd` on `/proc/self/ns`, and with a relative name after `chdir("/proc/self/ns")`. All must give the fixed canonical names in both verify runs; the `dirfd` and `chdir` forms test the gap above.
  Existing partial candidate: the case `readlink mount namespace` in `hermit-cli/tests/command_strict_verify.rs:300-305` runs GNU `readlink /proc/self/ns/mnt`; which of `readlink` or `readlinkat` that binary issues was not checked.
- T5 (covers F6): create a pipe, a socket pair, an eventfd, a memfd and an unlinked `O_TMPFILE`, and `readlinkat` each `/proc/self/fd/N`, both by absolute path and through a `dirfd` on `/proc/self/fd`. Output must be identical across verify runs; the `O_TMPFILE` case tests the gap above. Read a child's `/proc/<pid>/fd/N` from the parent too.
  Existing partial candidate: `tests/c/proc_fd_link_aliases.c:69-80` (pipe through a `dirfd` on `/proc/self/fd`, expected `readlinkat=pipe:[1001]` at `hermit-cli/tests/proc_fd_link_determinism.rs:147`), whose test runs ptrace, DBT and LiteInst, so it is not a ptrace-only target; cell `c-programs/proc-fd-link-aliases` (verify ci on ptrace).
- T6 (covers F7): `readlinkat` `/proc/self/exe`, `/proc/self/cwd` and `/proc/self/root`; output must be identical across verify runs from the same working directory.
- T7 (covers F8): a regular file (`EINVAL`), a missing path (`ENOENT`), a path through a regular file (`ENOTDIR`), a null `pathname` (`EFAULT`), an unmapped `buf` (`EFAULT`), and a symlink loop in a middle component (`ELOOP`).
- U1 (covers F9; host-transient): `EIO` and host `ENOMEM` depend on storage health and memory pressure shared with other processes. The handler forwards the kernel's result (`detcore/src/syscalls/namespace.rs:447`), so the error reaches the guest today. F9 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/394: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU, which disagrees with the code; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism deterministic -> conditional (procfs namespace, pipe and socket links embed host inodes); target_policy passthrough -> model (kernel reads the link; Detcore canonicalizes namespace links and self proc-fd pipe and socket links, per https://github.com/rrnewton/hermit/pull/877); 8 flag behaviours and 7 tests listed; gaps: AT_FDCWD-relative and dot-component paths are not canonicalized, no numeric-self alias rewrite unlike readlink, O_TMPFILE targets keep host inodes, bypassed under --passthru-opt; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant stays false (ordinary targets are filesystem input, procfs identities are canonicalized); split host EIO and ENOMEM out of F8 into new F9 with U1 host-transient; no open questions section

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for Detcore-decided /proc link targets, own-descriptor links on every backend, and invisible runtime descriptors.

## Discussion

[Start a discussion about `readlinkat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readlinkat&body=Discussion+for+the+%60readlinkat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadlinkat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readlinkat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: readlinkat" (https://github.com/rrnewton/hermit/issues/394), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=readlinkat` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `readlinkat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
readlinkat(AT_FDCWD, "/proc/self/exe", "/tmp/sysclass45/harness", 64) = 23
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
