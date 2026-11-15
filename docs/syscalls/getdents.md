---
schema: syscall-pages/v1
syscall: getdents
nr_x86_64: 78
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getdents&body=Discussion+for+the+%60getdents%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetdents.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getdents%60%2C+or+in+what+the+page+says%3F%0A
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

`getdents(fd, dirp, count)` is the legacy directory-read call: it fills `dirp` with `linux_dirent` records for the next entries of an open directory and returns the number of bytes written. glibc's `readdir` uses `getdents64` instead, so only raw callers and old static binaries reach number 78. Detcore handles it (`detcore/src/syscalls/files.rs:4558`): the kernel reads the directory, then Detcore sorts the entries inside each returned buffer by name and replaces every inode number with a deterministic one. The sort covers one buffer, not the whole directory, which is a known gap for large directories.

Manual reference: Linux man-pages getdents(2).

## Linux semantics

The call reads from the directory's current file position and advances it. Each record carries `d_ino`, `d_off` (an opaque cookie for the position after this entry, usable with `lseek`), `d_reclen`, a NUL-terminated name, padding, and the entry type in the record's last byte (`d_type` at `d_reclen - 1`). On x86_64 `d_ino` and `d_off` are 64-bit. The order of entries is whatever the filesystem's iterator produces: hash order on ext4 with htree, index order on btrfs, reverse creation order on tmpfs. `.` and `..` are included. A return of 0 means end of directory.

### Flags and options

- F1: `fd` must be an open directory: `EBADF` for a closed descriptor or one opened with `O_PATH`, `ENOTDIR` for a non-directory
- F2: `count` and `dirp`: the kernel writes as many whole records as fit and returns the byte count; `EINVAL` when the next record does not fit at all; `EFAULT` when `dirp` is not writable and an entry has to be copied out. At end of directory it returns 0 and never touches `dirp`.
- F3: legacy record layout: `d_ino`, `d_off`, `d_reclen`, `d_name`, padding, and `d_type` in the last byte of the record, unlike `getdents64`, which places `d_type` before the name
- F4: `d_type` values `DT_REG`, `DT_DIR`, `DT_LNK`, `DT_FIFO`, `DT_SOCK`, `DT_CHR`, `DT_BLK`, or `DT_UNKNOWN` on filesystems that do not report types
- F5: stream position and order: successive calls continue where the previous one stopped; entry order is filesystem-defined; `d_off` can be passed to `lseek` (and through glibc to `seekdir`) to resume after a given entry
- F6: concurrent change: an entry created or removed while the directory is being read may or may not appear (POSIX leaves it unspecified)
- F7: `d_ino` is the host inode number of each entry, including `.` and `..`
- F8: a directory that has been removed while still open returns `ENOENT`

## Determinism analysis

`linux_determinism: conditional`. For a fixed directory on a fixed filesystem the kernel returns the same entries in the same order every time, but that order, the `d_off` cookies and the inode numbers are host filesystem state: recreating the same names in another order, on another filesystem type, or on another machine changes all three. Concurrent creation or removal by another process changes which entries appear.

### Nondeterminism sources

- Filesystem iteration order, which depends on filesystem type, hash seed and creation history
- `d_off` cookies, which are hash values or offsets chosen by the filesystem
- Host inode numbers in `d_ino`
- `DT_UNKNOWN` versus a real type, depending on the filesystem
- Other processes changing the directory during iteration
- Buffer boundaries: which entries land in which call depends on `count` and on the order above

### Cross-host variation

- Filesystem type: whether `d_type` carries a real type or `DT_UNKNOWN` is passed through unchanged (F4).
- Filesystem type and its hash seed: the `d_off` cookies are written back as the host's raw values (`detcore/src/dirents.rs:169`), and for a directory larger than one buffer the host's walk order decides which names fall into each sorted run (F5). Both are stable for one directory on one host and differ on another filesystem.

## Hermit design

Target policy: `model`. The kernel remains the source of the directory's contents, and Detcore rewrites the parts that leak host state. The handler at `detcore/src/syscalls/files.rs:4558` does this today:

- When metadata virtualization is off (`--no-virtualize-metadata`, `detcore-model/src/config.rs:253`) the call goes to the kernel unchanged (`detcore/src/syscalls/files.rs:4563`).
- Otherwise it requires a non-NULL `dirp`, returning `EFAULT` before the kernel is asked (`detcore/src/syscalls/files.rs:4567`), then calls `record_or_replay` (`detcore/src/tool_local.rs:382`) and returns 0 unchanged at end of directory.
- It parses the returned bytes with the legacy-layout parser `deserialize_dirents` (`detcore/src/dirents.rs:123`), sorts them with `dents.sort()` (`detcore/src/syscalls/files.rs:4582`), which orders by name bytes with `.` and `..` first (`detcore/src/dirents.rs:31`), and maps each `d_ino` through `determinize_inode` (`detcore/src/syscalls/files.rs:4584`). Inode numbers are assigned in the order Detcore first sees each host inode (`recv_determinize_inode`, `detcore/src/tool_global.rs:2522`).
- It writes the re-serialized records back into a zeroed copy of the buffer and returns the kernel's byte count. Each record keeps its own `d_reclen` and `d_type`.

`getdents64` uses the same logic at `detcore/src/syscalls/files.rs:4598`.

Why `model` and not the alternatives: `emulate` would need Detcore to keep its own view of every directory, which is more than the problem requires, because the contents are correct and only the ordering and identities leak. `passthrough` would expose host order and host inodes, and is unavailable for a `determinized` syscall. `refuse` would break every program that lists a directory with the raw call.

What remains before the code fully realizes `model`:

- **Per-buffer sort.** The sort applies to one returned buffer. A directory larger than the caller's buffer comes back as several sorted runs, and which names fall into which run is host order. `tests/c/readdir_order_identity.c` records the measurement for `getdents64` through glibc: a 2000-entry directory gives `order_identical=0 seams_rev=1`, two sorted runs joined at the 32 KiB buffer boundary. Its header names the fix: drain and sort the whole stream at the first call, cache it per open file description, and synthesize monotonic `d_off` values. The same code path serves `getdents`, so the same gap applies.
- **Inode assignment follows the sort.** Because inode numbers are handed out in first-seen order, a large directory's buffer split also changes which virtual inode each entry gets.
- **`d_off` is not rewritten.** `serialize_dirents` writes each record's original `ent.off` (`detcore/src/dirents.rs:169`), so after sorting the cookies are raw host values and no longer increase along the returned order. A guest that saves a `d_off` and seeks back to it can skip or repeat entries. This is inferred from the code, not measured.
- **`EFAULT` ordering.** Linux checks `fd` first and does not touch `dirp` at end of directory, so `getdents(-1, NULL, n)` is `EBADF` and an exhausted stream returns 0 natively, while Hermit returns `EFAULT` for both. Inferred from the code at `detcore/src/syscalls/files.rs:4567`, not measured.
- `DT_UNKNOWN` (F4) is passed through, so filesystem type remains visible.

`linux_determinism` stays `conditional`: the kernel's answer depends on host filesystem state, and Hermit removes that dependency only for directories that fit in one buffer.

## Recommended tests

- T1 (covers F4, F5, F7): build the same name set twice in two creation orders, with mixed entry types, and compare the enumerations inside one run; a small directory must come back identical, and a large one must too once the per-buffer gap is fixed.
  Existing candidate: `tests/c/readdir_order_identity.c` (cell `c-programs/readdir-order-identity`, `tests/e2e/manifests/c-programs.yaml:17487`, verify ci on ptrace with `--require-small-determinized`). It goes through glibc `readdir`, so it exercises `getdents64`; a variant that calls `syscall(SYS_getdents, ...)` directly is needed for number 78.
- T2 (covers F2, F3): call `syscall(SYS_getdents, ...)` with a buffer that holds a few records and parse each record with `d_type` at `d_reclen - 1`; the names and types must match what `getdents64` returns for the same directory; a buffer smaller than one record gives `EINVAL`.
- T3 (covers F1, F2): native and Hermit must agree on `EBADF` for fd -1 with a NULL buffer, `ENOTDIR` for a regular file, `EBADF` for an `O_PATH` directory descriptor, and 0 for an exhausted directory read with a NULL buffer.
- T4 (covers F5): read a directory in several small calls, save the `d_off` of an entry in the middle, `lseek` to it, and check that reading resumes with the next entry in the order Hermit returned; this pins the `d_off` behaviour after sorting.
- T5 (covers F6, F8): create and unlink entries between two calls on one open directory and check that both runs under `run --verify` see the same entries; `rmdir` the directory while it is open and check that the next call returns `ENOENT`.
- T6 (covers F7): the `d_ino` of each entry equals the `st_ino` that `stat` returns for it under Hermit, and is the same in both runs of `run --verify`.
  Related existing test: `tests/c/readdir_entries.c` (cell `c-programs/readdir-entries`, `tests/e2e/manifests/c-programs.yaml:13346`), which checks names and types through `readdir` but sorts the names in the fixture and does not compare inode numbers.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (kernel reads the directory; Detcore sorts each returned buffer by name and determinizes d_ino); linux_determinism stays conditional; 8 flag behaviours and 6 tests listed; open questions on the per-buffer sort, raw d_off and EFAULT ordering recorded; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem type and hash seed: DT_UNKNOWN, raw d_off cookies, multi-buffer run boundaries); no U items; no open question resolved

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getdents`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getdents&body=Discussion+for+the+%60getdents%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetdents.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getdents%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getdents`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

No test at this SHA calls `SYS_getdents` (number 78) directly; every directory-listing test reaches `getdents64` through glibc.

### Open questions

- Should the whole-stream sort described in `tests/c/readdir_order_identity.c` (drain at the first call, cache per open file description, synthesize monotonic `d_off`) replace the per-buffer sort? It would fix ordering, inode assignment and `d_off` together, at the cost of memory proportional to directory size.
- Until then, should `d_off` be rewritten to a value that at least increases along the returned order, so `lseek`/`seekdir` stays consistent within one buffer?
- Should the `dirp` NULL check move after the kernel call, so `EBADF` and the end-of-directory 0 match Linux?
