---
schema: syscall-pages/v1
syscall: readahead
nr_x86_64: 187
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/393
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readahead&body=Discussion+for+the+%60readahead%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadahead.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readahead%60%2C+or+in+what+the+page+says%3F%0A
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

`readahead(fd, offset, count)` asks the kernel to start reading the pages of a file range into the page cache, so that later reads hit memory. It copies no data to the caller and returns only 0 or an errno. Hermit forwards it to the host kernel unchanged; the only state it changes, page-cache residency, belongs to the host.

## Linux semantics

The kernel resolves `fd`, requires it to be open for reading, and requires a file whose data lives in the page cache (a regular file or a block device). It then issues `POSIX_FADV_WILLNEED`-style readahead for the range and returns 0. The return value does not say how many pages were read, whether any I/O was needed, or whether the range lies past end of file. How much is actually read is capped by the device's maximum readahead size, and the call may block while it submits the reads. The file offset, the file data and the inode metadata do not change. A failure to read pages is not reported to the caller.

Errno results observed on the host (Linux 7.1, btrfs, raw `syscall(187, ...)` probe; see Additional information): `EBADF` for a closed fd, an `O_WRONLY` fd and an `O_PATH` fd; `EINVAL` for a negative offset, for `count = (size_t)-1`, and for a pipe, a socket, a directory and `/dev/null`; 0 for a regular file at offset 0, for `count = 0`, for an offset past end of file, and for `/proc/self/status` (a procfs regular file).

### Flags and options

- F1: `fd` is a regular file (or block device) open for reading: readahead of `[offset, offset + count)` is scheduled and the call returns 0
- F2: `count = 0`, or a range that starts past end of file: the call still returns 0. The amount read is a host decision and is not reported.
- F3: a negative `offset`, or a `count` so large that `offset + count` overflows `loff_t` (for example `(size_t)-1`): `EINVAL`
- F4: `fd` is not an open descriptor, or is not open for reading (`O_WRONLY`, `O_PATH`): `EBADF`
- F5: `fd` refers to a file without page-cache backing for readahead (pipe, socket, directory, character device such as `/dev/null`): `EINVAL`
- F6: side effects: none on the file offset, the file data or the inode metadata. The only effect is page-cache residency, which a guest can observe only indirectly (through `mincore(2)` on a file mapping, `preadv2(..., RWF_NOWAIT)` succeeding instead of returning `EAGAIN`, or timing).

## Determinism analysis

The return value is a function of the descriptor's file type and open mode and of the two integer arguments. It does not depend on what is already cached, on the file's size, or on timing. Under Hermit the descriptor table is the guest's own and threads run one at a time, so the call repeats whenever the guest reaches it with the same descriptor and arguments.

### Nondeterminism sources

- None in the result.
- The side effect (F6) changes host page-cache state, which other processes on the host also change. That state is visible only through other syscalls (`mincore`, `preadv2` with `RWF_NOWAIT`) and through timing. Making those observations deterministic is the job of the tracking issues for those syscalls, not of this one; passing `readahead` through neither adds nor removes that exposure.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `readahead` is classified `PassThrough` at `detcore/src/syscall_classification.rs:797`, inside the block that opens at `detcore/src/syscall_classification.rs:690` and ends with `=> SyscallClassification::PassThrough` at `detcore/src/syscall_classification.rs:804`. The group comment at `detcore/src/syscall_classification.rs:767` states the assumption: "Stable guest-owned metadata and synchronous writeback operations are repeatable in Hermit's fixed mount namespace and filesystem image." The census test lists it in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1735`, and `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the classification at `detcore/src/syscall_classification.rs:2094`. The generic arm at `detcore/src/lib.rs:2843` sends it to `Detcore::passthrough` (`detcore/src/lib.rs:367`), which forwards the call through `record_or_replay_preserving_tool_errors` without changing it.

Why passthrough:

- The result carries no host-dependent value (see Determinism analysis), so there is nothing to model.
- Emulating it as a no-op returning 0 would be wrong for F3 to F5 unless Hermit reimplemented the kernel's file-type and open-mode checks. Forwarding gets them right for free.
- Dropping the hint would change no guest-visible value but would cost real I/O performance for programs (databases, loaders) that rely on it.

Run mode subscribes every syscall (`Subscription::all_syscalls()` at `detcore/src/lib.rs:1094`), so `readahead` does reach Detcore and is logged. Code facts show `subscribed_record_replay: false`: under `--passthru-opt`, which the comment at `detcore/src/lib.rs:3140` says `hermit record` and `hermit replay` turn on, the call never reaches Detcore and runs directly against the kernel. That is harmless here because the result needs no recording.

Existing evidence: `tests/c/syscall_file_metadata.c:218` calls `syscall(SYS_readahead, fd, 0, 4096)` on a regular file and requires 0. The cargo test `hermit-cli/tests/syscall_file_metadata.rs:51` runs it on `--backend=ptrace` under `run --strict --panic-on-unsupported-syscalls`, asserts that the trace contains `inbound syscall: readahead(` (`hermit-cli/tests/syscall_file_metadata.rs:97`), and then requires `Determinism verified` from `run --strict --verify` (`hermit-cli/tests/syscall_file_metadata.rs:127`). The same program is the e2e cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`, verify mode with `ptrace: true` at `tests/e2e/manifests/c-programs.yaml:9294`). This covers F1 only, so verification is `partial`, not `verified`. This agent read the tests and did not run them.

## Recommended tests

- T1 (covers F1, F2): on a regular file opened `O_RDONLY`, call raw `syscall(SYS_readahead, ...)` with `(0, 4096)`, `(0, 0)` and an offset past end of file. Assert 0 each time.
  Existing coverage of the `(0, 4096)` case: `tests/c/syscall_file_metadata.c:218`, run on ptrace by `hermit-cli/tests/syscall_file_metadata.rs` and by the cell `c-programs/syscall-file-metadata`.
- T2 (covers F3): assert `EINVAL` for `offset = -1` and for `count = (size_t)-1`.
- T3 (covers F4): assert `EBADF` for a closed descriptor, an `O_WRONLY` descriptor and an `O_PATH` descriptor.
- T4 (covers F5): assert `EINVAL` for the read end of a pipe, an `AF_UNIX` socket, a directory opened `O_RDONLY|O_DIRECTORY`, and `/dev/null`.
- T5 (covers F6): around a successful `readahead`, assert that `lseek(fd, 0, SEEK_CUR)`, the file's bytes and its `st_size` are unchanged. Run the program under `hermit run --verify` on ptrace so that both runs must agree.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/393: linux_determinism deterministic (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept deterministic/passthrough (forwarded at detcore/src/lib.rs:2843; the result carries no host value); listed 6 flag behaviours and 5 tests; ptrace_verification untested -> partial because hermit-cli/tests/syscall_file_metadata.rs runs SYS_readahead (F1 only) on ptrace under --strict --verify; no coverage token recorded, tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `readahead`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+readahead&body=Discussion+for+the+%60readahead%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Freadahead.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60readahead%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: readahead" (https://github.com/rrnewton/hermit/issues/393), legacy variant A1.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(187, ...)` through Python `ctypes` on this host (Linux 7.1.3, btrfs scratch directory, unprivileged uid). Not run under Hermit.

```text
readahead reg 0 4096           -> 0
readahead reg count 0          -> 0
readahead reg off past EOF     -> 0
readahead reg off -1           -> -1 EINVAL
readahead reg count huge       -> -1 EINVAL
readahead O_WRONLY             -> -1 EBADF
readahead bad fd               -> -1 EBADF
readahead O_PATH               -> -1 EBADF
readahead pipe                 -> -1 EINVAL
readahead directory            -> -1 EINVAL
readahead socket               -> -1 EINVAL
readahead /dev/null            -> -1 EINVAL
readahead /proc/self/status    -> 0
```

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=readahead` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `readahead` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
readahead(3, 0, 4096)           = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

Legacy body claims superseded here: the 2026-07-24 Summary said only "Initiates non-blocking readahead"; the call can block while submitting reads, so this body says it "may block". The legacy Linux semantics line ("Verified: returns 0 on a regular file") is kept as F1.
