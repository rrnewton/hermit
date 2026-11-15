---
schema: syscall-pages/v1
syscall: getdents64
nr_x86_64: 217
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getdents64&body=Discussion+for+the+%60getdents64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetdents64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getdents64%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 10
---

## Summary

`getdents64(fd, dirp, count)` fills `dirp` with as many `struct linux_dirent64` records from the open directory `fd` as fit in `count` bytes and returns the number of bytes written, or 0 at the end of the directory. Detcore handles it (`detcore/src/syscalls/files.rs:4598`): it forwards the call through `record_or_replay`, then sorts the records of each returned buffer by name and replaces every host inode number with a deterministic one. The sort covers one buffer at a time, so a directory that needs more than one call still leaks host order at the buffer boundary.

Manual reference: Linux man-pages getdents(2), readdir(3), seekdir(3).

## Linux semantics

The kernel walks the directory from the position stored in the open file description and emits one record per entry: `d_ino` (inode number), `d_off` (an opaque filesystem cookie for the position after this entry), `d_reclen` (record length, padded to 8 bytes), `d_type` (a `DT_*` value) and the NUL-terminated `d_name`. It stops when the next record does not fit, advances the file position, and returns the byte count. glibc's `readdir` calls it with a 32 KiB buffer; `telldir` returns the `d_off` of the last record handed out, and `seekdir`/`rewinddir` call `lseek` on the directory fd with such a cookie or with 0.

Entry order is whatever the filesystem's directory walk produces: hash order on ext4 htree directories (with a per-filesystem hash seed), creation-dependent order on tmpfs, B-tree key order on btrfs, and a generated order on procfs. "." and ".." are ordinary records in that order; Linux does not promise to return them first. `d_type` is `DT_UNKNOWN` on filesystems that do not store the type. Entries created or removed while a directory is being read may or may not appear.

Errors: `EBADF` for a bad fd, `ENOTDIR` when `fd` is not a directory, `EINVAL` when `count` is too small for the next record, `EFAULT` for a bad buffer, and `ENOENT` when the directory has been removed.

### Flags and options

- F1: normal read: the call returns a positive byte count holding one or more complete records, each with `d_ino`, `d_off`, `d_reclen`, `d_type` and `d_name`, and advances the directory position.
- F2: end of directory: once every entry has been returned, the call returns 0.
- F3: directory larger than one buffer: a full enumeration needs several calls, and the guest sees the concatenation of the batches in the filesystem's walk order.
- F4: "." and ".." entries appear in the stream; their position is filesystem order, not guaranteed first.
- F5: `d_type`: `DT_REG`, `DT_DIR`, `DT_LNK` and the other types, or `DT_UNKNOWN` where the filesystem does not record the type.
- F6: `d_off` cookies with `lseek` on the directory fd (`telldir`/`seekdir`/`rewinddir`): seeking to a returned cookie resumes the walk after that entry; seeking to 0 restarts it.
- F7: `count` too small to hold the next record: `EINVAL`.
- F8: `dirp` invalid or NULL: `EFAULT`.
- F9: `fd` not open: `EBADF`; `fd` open but not a directory: `ENOTDIR`; directory already removed: `ENOENT`.
- F10: concurrent modification: entries created, removed or renamed between calls may or may not appear in later batches.
- F11: dynamic directories such as `/proc` and `/proc/<pid>/fd`, whose contents are generated at read time.

## Determinism analysis

`linux_determinism: nondeterministic`. The result is a readout of host filesystem state that is not guest input: the order of entries depends on the filesystem type, its hash seed and the directory's creation and deletion history, so the same name set enumerates in a different order on another machine or after a different creation order. `d_ino` values are host inode numbers, and `d_off` values are host hash cookies. The call repeats only when it runs twice against the same on-disk directory on the same filesystem with no concurrent writer.

### Nondeterminism sources

- Entry order (F1, F3, F4): filesystem hash order and creation history. `tests/c/readdir_order_identity.c:46-53` records a native run where the same name set created in two orders enumerated differently (`order_identical=0`).
- Inode numbers in `d_ino` (F1): host allocation.
- Position cookies in `d_off` (F6): host hash values.
- Concurrent modification by another thread or process (F10), and procfs contents that depend on host state such as process ids and open descriptors (F11).

A double-run `--verify` comparison cannot see an order leak, because both runs read the same on-disk layout; the fixture header at `tests/c/readdir_order_identity.c:13-19` explains this and why that probe compares two creation orders inside one run instead.

### Cross-host variation

- Filesystem type: whether `d_type` carries a real type or `DT_UNKNOWN` is passed through unchanged (F5).
- Filesystem type and its hash seed: the `d_off` cookies keep the host's raw values (F6), and for a directory larger than one buffer the host's walk order decides which names fall into each sorted run (F3). Both are stable for one directory on one host and differ on another filesystem.

## Hermit design

Target policy: `model`. The kernel still walks the directory, and Detcore rewrites what the guest sees so it depends only on the name set. The handler at `detcore/src/syscalls/files.rs:4598` (dispatch arm `detcore/src/lib.rs:2607`) does this today:

- With `virtualize_metadata` off, the call goes to the kernel unchanged through `record_or_replay` (`detcore/src/syscalls/files.rs:4603`). Metadata virtualization is on by default (the `--no-virtualize-metadata` switch at `detcore-model/src/config.rs:252` clears it) and `--strace-only` turns it off (`hermit-cli/src/bin/hermit/run.rs:3757`).
- A NULL `dirp` returns `EFAULT` before the kernel is asked (`detcore/src/syscalls/files.rs:4607`), which matches F8 for NULL; other bad pointers fail in the kernel. F2, F7 and F9 are kernel results passed back unchanged, and a return of 0 is returned without rewriting (`detcore/src/syscalls/files.rs:4610`).
- For a positive return, Detcore reads the `nb` bytes back, parses them (`deserialize_dirents64`, `detcore/src/dirents.rs:55`), sorts the records (`detcore/src/syscalls/files.rs:4622`), replaces every `d_ino` with the result of `determinize_inode` (`detcore/src/syscalls/files.rs:4624`, `detcore/src/tool_global.rs:3682`), and writes the buffer back into a zeroed copy (`detcore/src/syscalls/files.rs:4628-4633`). The byte count is unchanged. `d_type`, `d_reclen` and `d_off` travel with their record.
- The output buffer is hashed into the detlog (`detcore/src/io_buffers.rs:400`), so a later difference in its bytes shows up in a strict verification log.

Why `model` and not the alternatives: `passthrough` exposes host order and host inode numbers, which is exactly the leak above; `emulate` would need Detcore to own the directory contents, which it does not; `refuse` would break every program that lists a directory.

What remains before the code fully realizes `model`:

- **The sort is per buffer (F3).** Each call sorts only its own batch, so a directory that needs more than one call comes back as several sorted runs joined in host order. `tests/c/readdir_order_identity.c:46-53` records this under hermit ptrace: the 24-name probe is fully ordered, the 2000-name probe is two sorted runs joined at one seam (`seams_rev=1`). The header proposes the fix: drain and sort the whole stream at the first call, cache it per open file description, and synthesize monotonic `d_off` values (`tests/c/readdir_order_identity.c:55-60`).
- **`d_off` cookies are still host values (F6).** After the sort, each record keeps its own host cookie, so the cookies the guest sees are no longer increasing and their values come from the host. By this reading of the code, a `telldir` followed by `seekdir` resumes the kernel walk after that entry in host order, which can repeat or skip entries relative to the sorted order the guest saw. This is a hypothesis from the code, not a measurement; T5 confirms or refutes it.
- **The "." and ".." rule never fires (F4).** The comparison at `detcore/src/dirents.rs:31-47` puts a record named `b".\0"` or `b"..\0"` first, but the name slice built at `detcore/src/dirents.rs:74` is `reclen - 18` bytes long (the name, its NUL, the padding, and one more byte), so it never equals a two- or three-byte literal. By this reading, the order is a plain byte comparison of the names, and names that begin with a byte below `.` (for example `-x` or `+x`) sort before ".". The order is still independent of the host, but it is not the order the comment describes. The slice for the last record also reaches one byte past the `nb` bytes read, into the spare capacity reserved at `detcore/src/syscalls/files.rs:4615`. The unit tests at `detcore/src/dirents.rs:640` and `detcore/src/dirents.rs:666` check only round trips and lengths, not the order.
- **Concurrent modification and procfs (F10, F11)** are not modelled: whatever the kernel returns is sorted and renumbered, so the guest still sees host timing and host process state when the directory changes under it.

`linux_determinism` stays `nondeterministic`: Hermit removes the order and inode sources for directories that fit in one buffer, and the remaining sources are listed above.

## Recommended tests

- T1 (covers F1, F2, F5): list a fresh directory holding regular files, a subdirectory and a symlink; check the names, the count, each `d_type`, and that the final call returns 0, under `hermit run --verify` on ptrace.
  Existing candidate: `tests/c/readdir_entries.c` (cell `c-programs/readdir-entries`, `tests/e2e/manifests/c-programs.yaml:13346`, verify ci on ptrace). It covers three regular files and does not check subdirectories, symlinks or `DT_UNKNOWN`.
- T2 (covers F1, F3, F4): create the same name set in two orders inside one run and compare the two enumerations, for a directory that fits in one buffer and for one that needs several.
  Existing candidate: `tests/c/readdir_order_identity.c` (cell `c-programs/readdir-order-identity`, `tests/e2e/manifests/c-programs.yaml:17487`, verify ci on ptrace with `--require-small-determinized`). Its large probe pins the per-buffer gap and must be updated deliberately when the whole stream is sorted.
- T3 (covers F4): a directory holding names that begin with `-`, `+` and a space; record where "." and ".." land in the stream under Hermit, and decide the intended order before fixing `detcore/src/dirents.rs:31-47`.
- T4 (covers F7, F8, F9): with raw `syscall(SYS_getdents64, ...)`, check `EINVAL` for `count` 1, `EFAULT` for a NULL and for an unmapped `dirp`, `EBADF` for a closed fd, `ENOTDIR` for a regular file fd, and `ENOENT` after `rmdir` of the open directory.
- T5 (covers F6): on a directory that needs several calls, record `telldir` after each entry, `seekdir` back to a cookie in the middle, and check that the remaining entries are exactly the tail of the first enumeration with no repeats or gaps; also check `rewinddir` restarts at the first sorted entry.
- T6 (covers F10): one thread enumerates a large directory while another creates and unlinks files in it under `hermit run --verify`; both runs must see the same sequence.
- T7 (covers F11): list `/proc/self/fd` and `/proc` inside the guest and check that the output repeats across two runs.
  Related existing script: `tests/e2e/system-utils/ps-proc-table.sh`, which walks `/proc` through `ps`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel walks the directory; Detcore sorts each returned buffer and renumbers inodes); linux_determinism nondeterministic; gaps: per-buffer sort, host d_off cookies, dead "." rule; 11 flag behaviours and 7 tests listed; tracking_status stays todo with open questions
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and hash seed: DT_UNKNOWN, raw d_off cookies, multi-buffer run boundaries); no U items; no open question resolved

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getdents64`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getdents64&body=Discussion+for+the+%60getdents64%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetdents64.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getdents64%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getdents64`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

`tests/c/readdir_order_identity.c` came from commit 87c93e4f47 (2026-08-06), which recovered it from the closed pull request https://github.com/rrnewton/hermit/pull/1700. The legacy `getdents` handler at `detcore/src/syscalls/files.rs:4558` follows the same per-buffer pattern. `hermit-cli/tests/rr_suite.rs:403` registers `rr_readdir`, which is ignored at this SHA (`hermit-cli/tests/fail_closed_allowed_ignores.tsv:154`).

### Open questions

- Whole-stream ordering: should Detcore drain and sort the entire directory at the first call and cache it per open file description, as `tests/c/readdir_order_identity.c:55-60` proposes? That changes memory use for very large directories and needs a rule for entries created during the walk (F10).
- What should `d_off` hold once the stream is sorted: synthesized monotonic positions with `lseek` on directory fds translated by Detcore, or something else? This decides whether `seekdir` can ever be faithful.
- Should "." and ".." be forced first, as the comment at `detcore/src/dirents.rs:31` intends, or should the comment be corrected to plain byte order? Either is deterministic; the choice changes the guest-visible order.
