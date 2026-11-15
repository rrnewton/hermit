---
schema: syscall-pages/v1
syscall: readlink
nr_x86_64: 89
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readlink&body=Discussion+for+the+%60readlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readlink%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 3
---

## Summary

`readlink(path, buf, bufsiz)` copies the target of a symbolic link into `buf` without a terminating NUL and returns the number of bytes copied. For ordinary symlinks the answer is filesystem content; for the `/proc` magic links it exposes host inode numbers, namespace identities and paths. Detcore handles it (`detcore/src/syscalls/namespace.rs:455`): the kernel reads the link, and Detcore replaces namespace identities with fixed strings and pipe or socket descriptor targets with deterministic inode numbers. Every other target is returned as the kernel produced it.

Manual reference: Linux man-pages readlink(2) and proc(5).

## Linux semantics

The kernel resolves every component except the last, which must be a symlink. It copies at most `bufsiz` bytes of the target and silently truncates; the caller detects truncation only by a return value equal to `bufsiz`. `bufsiz <= 0` is checked before the path is looked up. A relative path resolves against the current directory, so `/proc/self/fd/3` and `3` with the current directory at `/proc/self/fd` read the same link.

The `/proc` magic links are synthesized by the kernel on each read: `/proc/<pid>/fd/<n>` and `/dev/fd/<n>` give the path of the open file (with ` (deleted)` when unlinked) or a `pipe:[<inode>]`, `socket:[<inode>]` or `anon_inode:[<type>]` string; `/proc/<pid>/ns/<type>` gives `<type>:[<inode>]`; `/proc/self` gives the caller's pid; `exe`, `cwd` and `root` give paths. Reading another process's links requires ptrace read access to it.

### Flags and options

- F1: ordinary symlink: copy up to `bufsiz` bytes of the stored target, no NUL, silent truncation, return the count
- F2: `bufsiz <= 0`: `EINVAL`, checked before the path is looked up
- F3: final component is not a symlink: `EINVAL`
- F4: path and buffer errors: `ENOENT`, `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, `EACCES` for a component without search permission, `EFAULT` for an unreadable path or an unwritable buffer
- F5: descriptor links `/proc/<pid|self|thread-self>/fd/<n>` and `/dev/fd/<n>`: the open file's path (with ` (deleted)` if unlinked), or `pipe:[<inode>]`, `socket:[<inode>]`, `anon_inode:[<type>]` for anonymous objects
- F6: namespace links `/proc/<pid>/ns/<type>` and `/proc/<pid>/task/<tid>/ns/<type>` for `cgroup`, `ipc`, `mnt`, `net`, `pid`, `pid_for_children`, `time`, `time_for_children`, `user` and `uts`: `<type>:[<inode>]`, equal for two processes exactly when they share that namespace
- F7: other magic links: `/proc/self` and `/proc/thread-self` (the caller's pid and tid as decimal strings), `/proc/<pid>/exe`, `cwd`, `root` and `map_files/*` (host paths)
- F8: another process's `/proc/<pid>/...` links: allowed only with ptrace read access to that process, otherwise `EACCES`
- F9: relative spellings: a relative path resolves against the current directory, so a magic link can be read without naming `/proc` in the path

## Determinism analysis

`linux_determinism: conditional`. An ordinary symlink returns the same bytes whenever the filesystem holds the same link. The magic links carry host state: inode numbers of pipes, sockets and namespaces, host paths of files and executables, and pids. These differ between hosts and between boots, and pipe and socket inodes differ between runs on the same host.

### Nondeterminism sources

- Host inode numbers in `pipe:[...]`, `socket:[...]` and namespace links
- Host paths in descriptor links and in `exe`, `cwd` and `root`, which depend on where the guest was started and on temporary directory names
- Pids in `/proc/self` and in other processes' paths
- Filesystem content for ordinary symlinks, which other processes can change

## Hermit design

Current behaviour (hermit 239ca43d3092): Detcore decides more `/proc` link targets itself instead of only rewriting the kernel's answer. A `readlink` of a `/proc` link whose target Detcore owns — the caller's own descriptor links (including `thread-self` spellings) and namespace links — is answered from Detcore's model on every backend without asking the backend (`readlink_decided_link`, `detcore/src/syscalls/namespace.rs:2055`), the path walk counts links and applies the kernel's lookup checks itself, system-directory links are decided by the object the walk reaches, and descriptors belonging to the in-guest runtime are invisible in the guest's numbering. Not in a nested PID namespace or after a guest `chroot`, where `/proc` need not be Detcore's. Ordinary filesystem symlinks and the namespace-identity rewriting described below are unchanged.

Target policy: `model`. The kernel reads the link; Detcore rewrites the parts of magic-link targets that carry host identities. The code at `detcore/src/syscalls/namespace.rs:455` (marked `TODO-HUMAN-REVIEW(#877)`, https://github.com/rrnewton/hermit/issues/877) does this:

- It reads the path, returning `EFAULT` for NULL before the kernel is asked (`detcore/src/syscalls/namespace.rs:460`).
- `proc_fd_target` (`detcore/src/syscalls/namespace.rs:130`) recognizes `/dev/fd/<n>` and `/proc/<self|thread-self|pid>/fd/<n>` after lexical normalization of `.` and `..` (`normalized_absolute_parts`, `detcore/src/syscalls/namespace.rs:98`).
- A numeric spelling of the caller's own pid is rewritten to `/proc/self/fd/<n>` on the guest stack before the kernel call (`host_self_proc_fd_alias`, `detcore/src/syscalls/namespace.rs:146`; rewrite at `detcore/src/syscalls/namespace.rs:470`).
- Another process's descriptor link goes to `finish_other_proc_fd_readlink` (`detcore/src/syscalls/namespace.rs:355`), which reads into 32 bytes of scratch (`ANONYMOUS_PROC_FD_TARGET_CAPACITY`, `detcore/src/syscalls/namespace.rs:160`) when the caller's buffer is too small to hold a full `socket:[<inode>]` string.
- Everything else goes through `finish_namespace_readlink` (`detcore/src/syscalls/namespace.rs:435`) and then `canonicalize_namespace_readlink_result` (`detcore/src/syscalls/namespace.rs:275`). A successful namespace link is replaced by a fixed string per type (`canonical_namespace_name`, `detcore/src/syscalls/namespace.rs:48`), for example `pid:[4026531836]`. A descriptor link whose `fstat` reports `S_IFIFO` or `S_IFSOCK` becomes `pipe:[<n>]` or `socket:[<n>]`, with `<n>` from the inherited-stdio overrides or from `determinize_inode` (`detcore/src/syscalls/namespace.rs:302`). Any other result is returned unchanged.
- The replacement is written truncated to the caller's `bufsiz` with no NUL, and the shortened length is returned (`detcore/src/syscalls/namespace.rs:324`), so truncation matches Linux.

Why `model` and not the alternatives: `passthrough` leaks host inode numbers into stdout of common tools (`readlink /proc/self/fd/1`, `ls -l /proc/self/fd`), which `proc_fd_link_consumers_verify` exists to catch, and is unavailable for a `determinized` syscall. `emulate` would require Detcore to answer ordinary symlinks itself, which the kernel already does correctly. `refuse` would break path canonicalization in nearly every runtime.

What remains before the code fully realizes `model`:

- **Relative spellings (F9).** `normalized_absolute_parts` returns `None` for a relative path (`detcore/src/syscalls/namespace.rs:99`), and the unit test `leaves_non_namespace_and_relative_links_untouched` (`detcore/src/syscalls/namespace.rs:548`) pins that. So `chdir("/proc/self/fd"); readlink("3", ...)` returns the raw host `pipe:[<inode>]`. The `openat` handler resolves relative spellings through the guest's cwd for this reason (comment at `detcore/src/syscalls/files.rs:986`); `readlink` does not. Inferred from the code, not measured.
- **Fixed namespace identities (F6).** Every process reports the same strings, so a guest that compares its namespace links with another process's, or with its own after `unshare`, always sees "same namespace". The values are deterministic but not faithful.
- **Host paths (F5, F7).** Regular-file descriptor targets, `exe`, `cwd` and `root` are returned as the kernel produced them. They repeat only when the guest is started from the same host paths.
- **Error ordering (F2).** `readlink(NULL, buf, 0)` is `EINVAL` on Linux and `EFAULT` under Hermit, because the path is read before `bufsiz` is checked (`detcore/src/syscalls/namespace.rs:460`). Inferred from the code, not measured.

`linux_determinism` stays `conditional`.

## Recommended tests

- T1 (covers F1, F3, F4): create a symlink and read it with a large buffer and with a buffer shorter than the target; check the counts, the bytes and the absence of a NUL. `readlink` on a directory and on a regular file gives `EINVAL`; a missing path gives `ENOENT`; a NULL buffer with a positive `bufsiz` gives `EFAULT`.
  Existing partial coverage: `readlink("/", ...)` returning `EINVAL` in `tests/c/errno_path_identity.c:96` (cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`).
- T2 (covers F2): `readlink(path, buf, 0)` and `readlink(NULL, buf, 0)` both give `EINVAL`, as natively.
- T3 (covers F5): a pipe descriptor read through `/proc/self/fd/<n>`, `/proc/<pid>/fd/<n>`, `/dev/fd/<n>`, `/proc/self/fd/../fd/<n>` and with a 10-byte buffer all give the same deterministic `pipe:[...]` string or its prefix, in both runs of `run --verify`.
  Existing candidates: `tests/c/proc_fd_link_aliases.c` (cell `c-programs/proc-fd-link-aliases`, `tests/e2e/manifests/c-programs.yaml:5817`, verify ci on ptrace); `proc_fd_link_consumers_verify` at `hermit-cli/tests/proc_fd_link_determinism.rs:71`, which runs `readlink`, `ls -l` and `stat` on `/proc/self/fd/1` with `--backend=ptrace`; and `proc_fd_link_aliases_and_truncation_verify` at `hermit-cli/tests/proc_fd_link_determinism.rs:115`, which expects `pipe:[1001]` for every spelling but loops over ptrace, dbt and liteinst.
- T4 (covers F6): `readlink("/proc/self/ns/pid")`, `/proc/self/task/<tid>/ns/mnt` and each other namespace type return the fixed strings in both runs; record what a child after `unshare(CLONE_NEWUTS)` reports for `uts` compared with its parent.
  Existing unit coverage: `recognizes_process_and_thread_namespace_links` at `detcore/src/syscalls/namespace.rs:532`, which tests only path recognition.
- T5 (covers F7): `readlink("/proc/self")`, `/proc/thread-self`, `/proc/self/exe` and `/proc/self/cwd` give the same results in both runs of `run --verify` and agree with `getpid`, `gettid` and `getcwd`.
- T6 (covers F5, F8): a parent reads a child's `/proc/<child>/fd/<n>` for a socket with a buffer smaller than 32 bytes and with a large one; both give the deterministic `socket:[...]` form. A process without ptrace access to the target gets `EACCES`, as natively.
- T7 (covers F9): `chdir("/proc/self/fd")` then `readlink("<n>")` on a pipe descriptor; record whether the result is the deterministic string or the raw host inode, and compare with the absolute spelling.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel reads the link; Detcore fixes namespace identities and determinizes pipe and socket descriptor targets); linux_determinism stays conditional; 9 flag behaviours and 7 tests listed; open questions on relative spellings, fixed namespace identities and EFAULT ordering recorded; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.
- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · updated for Detcore-decided /proc link targets, own-descriptor links on every backend, and invisible runtime descriptors.

## Discussion

[Start a discussion about `readlink`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readlink&body=Discussion+for+the+%60readlink%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadlink.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readlink%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `readlink`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`readlinkat` shares the canonicalization through its own handler (`detcore/src/syscalls/namespace.rs:502`) and is tracked separately.

### Open questions

- Should relative spellings of descriptor and namespace links be resolved through the guest's cwd before matching, as the `openat` handler does? Today they return raw host identities.
- Is one fixed identity per namespace type acceptable, or should Detcore assign identities per actual namespace so that comparisons between processes stay faithful after `unshare` or `setns`?
- Should the NULL-path check move after the `bufsiz` check so `EINVAL` wins, as on Linux?
