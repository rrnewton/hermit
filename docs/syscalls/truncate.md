---
schema: syscall-pages/v1
syscall: truncate
nr_x86_64: 76
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/500
  - https://github.com/rrnewton/hermit/issues/522
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+truncate&body=Discussion+for+the+%60truncate%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftruncate.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60truncate%60%2C+or+in+what+the+page+says%3F%0A
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

`truncate(path, length)` sets the size of the regular file named by `path` to `length` bytes, discarding data past the new end or extending the file with zero bytes. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and Detcore keeps no file state of its own.

## Linux semantics

The kernel resolves `path` against the calling process's working directory and root, following symlinks in every component, including the last. It then checks that the target is a regular file the caller may write, takes write access on the inode, breaks any lease on the file, and calls `do_truncate`. Shrinking discards the bytes past `length`; pages of a shared mapping that now lie wholly past the end raise `SIGBUS` when touched. Extending makes the new range read as zero bytes, as a hole on filesystems that support sparse files.

The call returns `0` or a negative errno. It does not report the new size, timestamps or block count; a guest sees those only through the stat family and later reads.

### Flags and options

- F1: `length` smaller than the current size: data past `length` is discarded. Touching a shared mapping past the new end raises `SIGBUS`.
- F2: `length` larger than the current size: the file is extended, and the new range reads as zero bytes
- F3: `length` equal to the current size: the data is unchanged and `0` is returned
- F4: path resolution: a relative `path` resolves against the working directory, and symlinks are followed in every component, including the last, so a symlink's target is truncated
- F5: write permission on the file, or search permission on a directory in the path, is denied: `EACCES`
- F6: `path` names a directory: `EISDIR`. It names another non-regular file, such as a FIFO, socket or device: `EINVAL`.
- F7: `length` is negative: `EINVAL`. F14 holds `EFBIG` past the filesystem's maximum file size.
- F8: growing past the caller's `RLIMIT_FSIZE`: `SIGXFSZ` is sent to the caller and the call returns `EFBIG`
- F9: a component is missing or `path` is empty: `ENOENT`. A prefix component is not a directory: `ENOTDIR`. A component or the path is too long: `ENAMETOOLONG`. Too many symlinks: `ELOOP`. `path` points outside the address space: `EFAULT`.
- F10: the file is an executable that some process is running: `ETXTBSY`
- F11: `EROFS`: the file is on a read-only mount. F15 and F16 hold the other host-dependent failures.
- F12: side effects the call does not return: `mtime` and `ctime` change when the size changes. Unless the caller has `CAP_FSETID` over the file, `S_ISUID` is cleared, `S_ISGID` is cleared when the group-execute bit is set, and a `security.capability` attribute is removed.
- F13: another open file description holds a lease on the file: the lease is broken first, and the call blocks until the holder releases it or the system's lease-break time passes. A signal during that wait gives `EINTR`.
- F14: `length` is larger than the maximum file size of the host filesystem behind the file: `EFBIG`.
- F15: the inode carries the immutable or append-only attribute, which only the host can set: `EPERM`.
- F16: `EIO`: an I/O error from host storage while updating the inode.

## Determinism analysis

`linux_determinism: deterministic`. The result is a function of the filesystem state along the path, the file's size and type, the caller's credentials and resource limits, and the mount flags. Nothing in the call depends on timing, except the F13 wait, which ends when another process releases its lease. Under Hermit, threads run one at a time, so even two guest threads truncating and writing the same file get a fixed order. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it.

The two filings disagreed on the class. https://github.com/rrnewton/hermit/issues/500 said deterministic; its retired duplicate https://github.com/rrnewton/hermit/issues/522 said CONDITIONAL, because "errors and existing contents depend on external filesystem state". Both describe the same dependency. This body counts the filesystem state as an input, as the mkdir tracking issue does, so deterministic stands, and the external dependencies are listed below.

### Nondeterminism sources

- Changes to the file or the path made outside the container, including concurrent writers that are not guest processes. Hermit does not try to make a changing host filesystem deterministic.
- Host resources and attributes behind F11, F14, F15 and F16: read-only remounts, the filesystem's maximum file size, immutable or append-only flags set by the host, and I/O errors.
- `ETXTBSY` (F10) and lease waits (F13) caused by processes outside the guest that run or lease the same file.
- Values the call changes but does not return: the timestamps and block count in F12. They become visible only through `stat` and similar calls, and are handled there.

### Cross-host variation

- Filesystem type behind the guest's file: its maximum file size decides where `EFBIG` starts (F14).
- Inode attributes set on the host: immutable or append-only files give `EPERM` (F15).

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4:

- `truncate` is classified `PassThrough` at `detcore/src/syscall_classification.rs:766`, inside the pass-through block that begins at `detcore/src/syscall_classification.rs:690`. The comment at `detcore/src/syscall_classification.rs:756` gives the condition: "These synchronous extent and pathname operations are repeatable for guest-owned files in a fixed namespace with adequate space and no external mutation."
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` hands the call to the record/replay layer through `record_or_replay_preserving_tool_errors`, which keeps a host errno as the guest's result.
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `truncate` as `PassThrough` in the list at `detcore/src/syscall_classification.rs:2089`.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The file's contents and size live in the kernel, and every later `read`, `mmap` and `stat` sees them there. Only the kernel can apply F1 to F13 the same way those calls see them.
- Every input that decides the result is either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image) or another syscall's concern. Timestamps belong to the stat family; `RLIMIT_FSIZE` is set by `setrlimit`, which is handled outside this issue.

Code facts show `subscribed_record_replay: false`, so under `--passthru-opt` Detcore never sees `truncate`. `hermit record` has its own subscriptions, in the recorder module that the comment at `detcore/src/lib.rs:1078` names; that module is not in the exported tree, so this body does not describe how record treats `truncate`.

Not analysed here: F13 makes the call block in the kernel until another process acts. If the lease holder is another guest process, Hermit must still let that process run while this thread waits in the host kernel. T8 checks this.

The open gap is test evidence. Two ptrace verify cells cover F1 and F2; nothing covers F3 to F13.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call raw `syscall(SYS_truncate, ...)` on a file holding `HELLOWORLD` with `length` 4, 8 and 8 again. Assert `st_size` after each call, that the retained prefix is intact, and that the extended range reads as zero bytes.
  Existing coverage: `tests/c/path_file_ops.c:77` (shrink) and `tests/c/path_file_ops.c:81` (grow, with the zero readback at `tests/c/path_file_ops.c:92`), through glibc `truncate`, which issues `SYS_truncate` on x86_64 (cell `c-programs/path-file-ops`, `tests/e2e/manifests/c-programs.yaml:12961`, verify ci on ptrace). Raw `SYS_truncate` at `tests/c/syscall_file_io.c:147`, after a `fallocate` to 8192 bytes, so it shrinks to 4096 where `fallocate` works (cell `c-programs/syscall-file-io`, `tests/e2e/manifests/c-programs.yaml:9214`).
  No test covers F3.
- T2 (covers F1): map a file `MAP_SHARED`, shrink it with `truncate`, and touch a page past the new end inside a `SIGBUS` handler that `siglongjmp`s out. Assert that the handler runs exactly once and that the run is identical across `--verify` runs.
- T3 (covers F4): `chdir` into a scratch directory, then truncate by a relative path and through a symlink to a file. Assert that the symlink's target changed size and the symlink did not.
- T4 (covers F5, F6, F9): assert `EACCES` for a file the guest cannot write (owned by an unmapped host uid); `EISDIR` for a directory; `EINVAL` for a FIFO; `ENOENT` for a missing file and for `""`; `ENOTDIR` for a regular file used as a prefix; `ENAMETOOLONG` for a 256-byte component; `ELOOP` for a symlink loop; `EFAULT` for `path = (char *)1`.
- T5 (covers F7, F8): assert `EINVAL` for `length = -1`. `EFBIG` from the filesystem's maximum file size is F14, recorded by U2. Lower `RLIMIT_FSIZE` to 4096 with `SIGXFSZ` caught, grow a file to 8192, and assert `EFBIG` plus exactly one `SIGXFSZ`.
- T6 (covers F10, F11): assert `ETXTBSY` for `truncate("/proc/self/exe", 0)`. Assert `EROFS` under a read-only mount the guest can see, recording the path in the test. Immutable-file `EPERM` and `EIO` are F15 and F16, recorded by U2 and U1.
- U1 (covers F16; host-transient): `EIO` depends on the health of host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F16 counts toward done only once Hermit hides it or reports it as an infrastructure error.
- U2 (covers F14, F15; host-config): the maximum file size belongs to the host filesystem, and the immutable and append-only attributes need `CAP_LINUX_IMMUTABLE` in the host's initial user namespace, which the guest lacks, so CI cannot choose these errors from inside the container. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:766`, `detcore/src/lib.rs:2843`).
- T7 (covers F12): create a file with mode `04755`, drop `CAP_FSETID` with `capset`, truncate it, and assert that `S_ISUID` is gone. Repeat with `CAP_FSETID` kept and assert that it stays. The guest runs as root in a user namespace, so it holds `CAP_FSETID` by default.
- T8 (covers F13): a guest child takes an `F_SETLEASE` read lease on a file it owns. The parent truncates the file; the child handles the lease signal and releases with `F_UNLCK`. Assert that the truncate completes with `0`, that the run does not hang, and that the outputs are identical across `--verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/500: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/522 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (the code forwards at detcore/src/lib.rs:2843; Hermit owns no file state); merged duplicate https://github.com/rrnewton/hermit/issues/522, whose CONDITIONAL class lost to deterministic because both filings name the same filesystem-state dependency; listed 13 flag behaviours and 8 tests; no coverage token is recorded, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (filesystem maximum file size, host inode attributes); F7 narrowed to EINVAL, new F14 EFBIG past the filesystem maximum (host-config U2); F11 narrowed to EROFS, new F15 immutable or append-only EPERM (host-config U2) and F16 EIO (host-transient U1); T5 no longer asserts the filesystem EFBIG; the open question on unreachable host errors is resolved by decision 1

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `truncate`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+truncate&body=Discussion+for+the+%60truncate%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftruncate.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60truncate%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: truncate" (https://github.com/rrnewton/hermit/issues/500), legacy variant A1.

Duplicate https://github.com/rrnewton/hermit/issues/522 was also filed 2026-07-24 as "Syscall classification: truncate" (legacy variant A1) and is retired into this issue. Its comment history (0 comments) stays on that issue; the comment history of https://github.com/rrnewton/hermit/issues/500 stays on that issue. Both recommended **PASS-THRU**. The only contradiction was the determinism class, reconciled under Determinism analysis. Its manual reference was [truncate(2)](https://man7.org/linux/man-pages/man2/truncate.2.html).

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/500)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=truncate` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `truncate` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`. (Verbatim from 2026-07-24; superseded by Hermit design above.) The legacy body also recorded: "Verified: truncating a /tmp file to 8 bytes returned 0."

Observed strace line:
```
truncate("/tmp/sysclass45/tf", 8) = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/522)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=truncate`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `truncate` in the UNCLASSIFIED temporary-passthrough block. (Verbatim from 2026-07-24; superseded by Hermit design above.) The temporary file was truncated from three bytes to one.

Observed strace line:
```text
truncate("/tmp/hermit-b5-truncate-7bGEkR", 1) = 0
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*
