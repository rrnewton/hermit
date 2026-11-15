---
schema: syscall-pages/v1
syscall: creat
nr_x86_64: 85
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+creat&body=Discussion+for+the+%60creat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcreat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60creat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`creat(path, mode)` is the historical spelling of `open(path, O_CREAT|O_WRONLY|O_TRUNC, mode)`: it creates a file or truncates an existing one and returns a write-only descriptor. Detcore handles it by converting the call to `openat(AT_FDCWD, path, O_CREAT|O_WRONLY|O_TRUNC, mode)` at the dispatch arm (`detcore/src/lib.rs:2364`) and running its `openat` handler (`detcore/src/syscalls/files.rs:979`), so the kernel sees `openat` and Detcore records the new descriptor in its fd table. No test at this SHA calls `creat`.

Manual reference: Linux man-pages open(2), which documents `creat`.

## Linux semantics

The kernel resolves `path` relative to the current directory, follows a final symlink, and either creates a regular file or opens the existing object. A new file gets the permission bits of `mode` with the process umask (or the parent's default ACL) applied, the caller's filesystem uid, and either the caller's filesystem gid or the parent directory's gid when that directory is set-group-ID. Creating a file updates the parent directory's modification and change times; truncating an existing file updates the file's. The descriptor is the lowest free number, is opened write-only, has no close-on-exec flag, and on x86_64 carries `O_LARGEFILE`.

### Flags and options

- F1: create-or-truncate: a missing regular file is created; an existing regular file is opened and truncated to length 0; the result is a write-only descriptor at the lowest free number
- F2: `mode`: the permission bits (including `S_ISUID`, `S_ISGID`, `S_ISVTX`) for a newly created file, reduced by the umask or by the parent's default ACL; ignored when the file already exists. The kernel clears `S_ISGID` on the new file when the caller is not in its group.
- F3: metadata side effects: a new file's owner, group and three timestamps come from the caller and the clock; the parent directory's mtime and ctime change; truncating an existing file sets its mtime and ctime
- F4: existing non-regular targets: a directory gives `EISDIR`; a device is opened and the truncation is ignored; a FIFO open blocks until a reader opens the other end (a signal interrupts it with `EINTR`); a dangling final symlink creates its target
- F5: path and permission errors: `EFAULT` for an unreadable path, `ENOENT` for a missing directory component or an empty path, `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, `EACCES` without search permission on a component or write permission on the file or parent, `EROFS`, `ETXTBSY` when truncating a running executable, `EPERM` for an append-only or immutable file, `ENOSPC` or `EDQUOT`, `EMFILE` or `ENFILE` when no descriptor is available
- F6: no flags argument: `creat` cannot ask for `O_CLOEXEC`, `O_EXCL`, `O_NONBLOCK` or `O_APPEND`, so the descriptor always has those clear

## Determinism analysis

`linux_determinism: conditional`. Given the same filesystem contents, umask, credentials and descriptor table, `creat` makes the same decisions every time. What it writes into the filesystem includes the current time and a fresh host inode number, and whether it creates or truncates depends on what is already on disk, which other processes can change. A FIFO target makes the call wait for another process.

### Nondeterminism sources

- Prior filesystem state: whether the file exists, its type, permissions and whether a running process maps it (`ETXTBSY`)
- Host clock values written into the new or truncated file's timestamps and the parent directory's
- Host inode numbers for new files
- Other processes creating, removing or opening the same path, and the arrival of a reader on a FIFO
- Free space and quota

## Hermit design

Target policy: `model`. The kernel creates the file; Detcore keeps the descriptor's type, flags and path in its fd table and determinizes the metadata the guest later reads through `stat` and `getdents`.

What the code does today:

- The dispatch arm converts `Creat` into `Openat` (`detcore/src/lib.rs:2364`) using reverie's conversion, so everything below is the `openat` handler with flags `O_CREAT|O_WRONLY|O_TRUNC`.
- The handler reads the path, returning `EFAULT` for NULL (`detcore/src/syscalls/files.rs:984`), and resolves a relative spelling through the guest's root and cwd so later classification sees the real object (`detcore/src/syscalls/files.rs:993`).
- It asks the scheduler for the path resource with `Permission::R` (`detcore/src/syscalls/files.rs:1005`), then sends `Syscall::Openat(call)` through `record_or_replay` (`detcore/src/syscalls/files.rs:1008`; `detcore/src/tool_local.rs:382`).
- On success it types the descriptor: `/dev/random` and `/dev/urandom` become `FdType::Rng` (`detcore/src/syscalls/files.rs:1013`); a reopened scheduler-managed pipe becomes `FdType::Pipe` with a hidden physical `O_NONBLOCK` (comment at `detcore/src/syscalls/files.rs:1020`, https://github.com/rrnewton/hermit/issues/1850); everything else is regular. It registers the descriptor with `add_fd` (`detcore/src/syscalls/files.rs:1050`), classifies procfs spellings, stores the observed path (`detcore/src/syscalls/files.rs:1120`), and releases the resource.
- Errors are returned as the kernel produced them; the arm carries `// TODO: audit for error-nondeterminism:` (`detcore/src/syscalls/files.rs:1129`).
- Timestamps and inode numbers are not changed by this call. They are virtualized when read: `determinize_inode` assigns a deterministic inode and an epoch first-seen time (`detcore/src/tool_global.rs:2522`).

Why `model` and not the alternatives: the descriptor must enter Detcore's fd table or later `read`, `write`, `fcntl` and `close` handling would not know it, which rules out `passthrough` (unavailable anyway for a `determinized` syscall). `emulate` would mean running a private filesystem. `refuse` would break old programs and build tools that still call `creat`.

What remains before the code fully realizes `model`:

- **Resource permission.** `creat` creates and truncates, but the handler asks for read permission on the path (`detcore/src/syscalls/files.rs:1005`) because it serves every `openat`. If the scheduler uses this permission to order conflicting file operations, a truncating `creat` racing a reader of the same path would be treated as non-conflicting. Whether that matters depends on how the scheduler uses `Permission`, which has not been checked for this body.
- **FIFO targets (F4).** The `/dev/fd` pipe reopen path covers only pipes Detcore already manages; a named FIFO keeps regular handling (comment at `detcore/src/syscalls/files.rs:1030`). A blocking open of a FIFO waits inside the kernel where the sequentialized scheduler cannot see it. By the same reasoning as the known FIFO-open deadlock under `--strict`, a `creat` on an existing FIFO whose reader is another guest process can hang the container. Inferred, not measured.
- **Recorded call name.** Record and replay see `openat`, not `creat`. Replay correctness is the same as for `openat`; anything that reports the syscall by name (logs, `--verify` diffs) shows `openat`.

`linux_determinism` stays `conditional`.

## Recommended tests

- T1 (covers F1, F6): `creat` a new file; the descriptor is the lowest free number, `F_GETFL & O_ACCMODE` is `O_WRONLY`, `F_GETFD` has no `FD_CLOEXEC`, `O_APPEND` and `O_NONBLOCK` are clear, and `read` on it gives `EBADF`; `write` then `stat` reports the written size.
- T2 (covers F2, F3): with umask 022, `creat(path, 0777)` yields mode 0755; a second `creat` with mode 0600 on the existing file leaves 0755; `stat` of the file and its parent gives the same timestamps and inode number in both runs of `run --verify`.
- T3 (covers F1, F3): write data into an existing file, `creat` it again, and check length 0 and that the old data is gone; the file's mtime as Hermit reports it is the same in both runs.
- T4 (covers F4): `creat` on a directory gives `EISDIR`; on `/dev/null` it succeeds and the file stays a character device; through a dangling symlink it creates the target. Separately, with a bounded timeout, `creat` a FIFO in one guest process while another opens it for reading, and record whether Hermit completes, refuses or hangs.
- T5 (covers F5): `EFAULT` for a NULL path, `ENOENT` for a missing parent and for `""`, `ENOTDIR` through a regular-file component, `ENAMETOOLONG`, `ELOOP` through a symlink loop, and `EMFILE` after lowering `RLIMIT_NOFILE`; native and Hermit must agree.
- T6 (covers F1, F2): record a guest that calls `creat` and writes to the file, then replay it, and check that replay reproduces the guest-visible results.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (dispatch rewrites creat to openat; kernel creates the file and Detcore registers the descriptor); linux_determinism stays conditional; 6 flag behaviours and 6 tests listed, none existing; open questions on the read-only resource request and FIFO targets recorded; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `creat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+creat&body=Discussion+for+the+%60creat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcreat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60creat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `creat`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`creat` is in Detcore's optional pass-through subscription list (`detcore/src/lib.rs:1121`) and is charged in the open/close time group (`detcore/src/syscall_time.rs:99`). No file under `tests/` or `hermit-cli/tests` at this SHA calls `creat` or `SYS_creat`, so every test above is new.

### Open questions

- Should the `openat` handler request write permission when the flags include `O_CREAT`, `O_TRUNC` or a write access mode, so the scheduler treats `creat` as a writer of the path? It requests `Permission::R` for every open today.
- Should a blocking open of a named FIFO (through `creat` or `openat`) be probed or refused instead of waiting in the kernel?
