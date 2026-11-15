---
schema: syscall-pages/v1
syscall: copy_file_range
nr_x86_64: 326
linux_determinism: conditional
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/342
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+copy_file_range&body=Discussion+for+the+%60copy_file_range%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcopy_file_range.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60copy_file_range%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`copy_file_range(2)` copies bytes from one file to another inside the kernel, without passing the data through user space. Hermit refuses every call with `ENOSYS` before the kernel sees it, as if the host kernel predated the call (Linux 4.5). Programs that support older kernels then fall back to ordinary `read` and `write`, which Detcore already makes deterministic.

## Linux semantics

The call is `copy_file_range(fd_in, off_in, fd_out, off_out, len, flags)`. It copies up to `len` bytes and returns the number copied, which may be fewer than `len`; 0 means end of input. A filesystem may do the copy by sharing blocks (reflink), by a server-side copy (NFS, SMB) or by a generic in-kernel copy; the bytes written are the same in every case. Which of these happen, and whether a copy between two different filesystems is allowed, has changed across kernel releases (5.3 and 5.19).

### Flags and options

- F1: descriptor types: `fd_in` must be open for reading and `fd_out` for writing, both regular files. A bad descriptor or an `fd_out` opened with `O_APPEND` gives `EBADF`; a directory gives `EISDIR`; a pipe, socket or other non-regular file gives `EINVAL`.
- F2: `off_in` or `off_out` is `NULL`: the copy uses and advances that descriptor's file offset, which is shared with every duplicate of the descriptor.
- F3: `off_in` or `off_out` points to an offset: the copy uses and updates `*off`, and the file offset is left alone.
- F4: `flags`: must be 0, otherwise `EINVAL`.
- F5: the same file as input and output with overlapping ranges gives `EINVAL`.
- F6: two different filesystems: allowed from Linux 5.3; since 5.19 allowed only between filesystems of the same type that support it, and `EXDEV` otherwise.
- F7: lengths: `len` 0 returns 0; input at end of file returns 0; the result may be a short count.
- F8: other errors: `EFBIG` and `EOVERFLOW` (ranges past the file size limits), `ENOSPC` and `EDQUOT` (no space or quota), `ETXTBSY` (output is a running executable), `EPERM` (immutable or sealed output), `EFAULT` (bad offset pointer) and `EIO`.

## Determinism analysis

The bytes copied are a function of the input file. The return value and error depend on several things that can differ between runs or hosts:

- the descriptor types and the offsets;
- filesystem support;
- the kernel version (F6);
- free space and quotas;
- signals arriving during a long copy, which can shorten the count.

The file offset in the `NULL` case is shared, so a concurrent user of the same descriptor sees a timing-dependent position. `linux_determinism` is `conditional`.

### Nondeterminism sources

- Partial I/O and storage capacity
- Shared file offsets
- Filesystem/device-specific behavior

## Hermit design

Target (owner decision 9, 6): `refuse` with a fixed `ENOSYS`, the answer of a kernel older than 4.5, returned before any argument is examined, so a valid copy and a garbage-argument call get the same `ENOSYS`. Linux's `copy_file_range` checks in this order: nonzero `flags` gives `EINVAL`; then the descriptors are looked up (`EBADF`); then `generic_copy_file_checks` rejects non-regular files (`EISDIR`, `EINVAL`), an `O_APPEND` output (`EBADF`), overlapping ranges in one file (`EINVAL`) and offsets past the size limits (`EOVERFLOW`, `EFBIG`); then the copy runs. The fixed `ENOSYS` comes before all of these. A kernel without the call refuses every caller, so decision 6's unprivileged-success exception does not apply. The supportable fallback is ordinary `read` and `write`, which Detcore makes deterministic: GNU coreutils `cp` and Rust's `std::fs::copy` both treat `ENOSYS` from `copy_file_range` as "unavailable" and copy through user space (recalled, not re-checked in their sources for this body). glibc 2.30 and later forward the syscall's `ENOSYS` to the caller unchanged; its user-space emulation existed only in glibc 2.27 to 2.29, so the fallback is the caller's, not glibc's. Owner decision 9 makes `refuse` the long-term target, so no `sendfile`-style model is planned. Today's code already realizes the target, so no code change is needed. Code at hermit f4055eb9cae4:

- F1-F8: the arm at `detcore/src/lib.rs:2378` returns `Err(Error::Errno(Errno::ENOSYS))` without reading any argument and without calling the kernel. The comment at `lib.rs:2376-2377` gives the intent: "Present a stable pre-4.5-kernel boundary so callers use determinized read/write copying."
- The same reason is given at `detcore/src/syscall_classification.rs:492-495`, which lists `copy_file_range` as determinized.
- `is_deterministically_refused_syscall` names it explicitly at `syscall_classification.rs:1338`.
- The refusal arrived with https://github.com/rrnewton/hermit/pull/887 (the `PR-887` tag in both comments).
- Because no argument is checked, a bad descriptor, non-zero `flags` and overlapping ranges all give `ENOSYS` rather than `EBADF` or `EINVAL`. The output file and the offsets are never touched.
- Before owner decision 9, a model in the style of `sendfile` was considered possible: `handle_sendfile` (`detcore/src/syscalls/files.rs:2145-2223`) mediates copies between regular files and memfds, and returns `ENOSYS` for other endpoints. Decision 9 settles `copy_file_range` as `refuse`.

## Recommended tests

- T1 (covers F2, F4): both offsets `NULL` and `flags` 0 between two regular files; the call gives `ENOSYS` in every run under `hermit run --verify` on ptrace.
  Existing candidates: `tests/c/copy_file_range_refusal_probe.c` (cell `c-programs/copy-file-range-refusal-probe` at `tests/e2e/manifests/c-programs.yaml:652`, verify ci on ptrace), and the cargo test `copy_file_range_refusal_is_deterministic` (`hermit-cli/tests/copy_file_range_refusal.rs:12-60`). The cargo test runs that probe with `--backend=ptrace run --strict --verify`.
- T2 (covers F3, F7): explicit offsets with a 6-byte payload. The result is `ENOSYS`, both offsets are unchanged, and the destination stays empty. Add `len` 0 and an input offset at end of file; both also give `ENOSYS`.
  Existing candidate: `tests/c/copy_file_range_refusal.c` (cell `c-programs/copy-file-range-refusal` at `tests/e2e/manifests/c-programs.yaml:11318`, verify ci true with ptrace enabled), which checks the explicit-offset case and prints `copy_file_range_refusal src=6 dst=0 checksum=597 ok=3`.
- T3 (covers F1, F5): a closed descriptor, a pipe, a directory, an `O_APPEND` output and an overlapping range in one file. All give `ENOSYS`, never the kernel's `EBADF`, `EISDIR` or `EINVAL`, in every run.
- T4 (covers F4, F6): non-zero `flags`, and a copy from a tmpfs file to a file on another filesystem, both give `ENOSYS`.
- T5 (covers F8): a high-level copy of a file larger than 1 MiB (for example `cp`, or a Rust `std::fs::copy` program) finishes through the read/write fallback with identical output and file contents across runs. The `ENOSYS` answer also means the guest never sees `EFBIG`, `ENOSPC` or `ETXTBSY` from this call.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/342#issuecomment-5872957345
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/342: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; copy_file_range records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/342 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (every call returns fixed ENOSYS before the kernel, so callers fall back to determinized read/write); linux_determinism stays conditional; legacy batch-rank text replaced; 8 flag behaviours and 5 tests listed, two existing ptrace refusal programs and one ptrace-only cargo test named as candidates; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9 and 6 applied (fixed ENOSYS, as on a pre-4.5 kernel, before any argument check, already realized at detcore/src/lib.rs:2378; kernel check order recorded); target_policy stays refuse; no new F/T ids; removed the open question on a sendfile-style model and its heading
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/copy_file_range_refusal_probe.c, T1 covered by hermit-cli/tests/copy_file_range_refusal.rs (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `copy_file_range`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+copy_file_range&body=Discussion+for+the+%60copy_file_range%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fcopy_file_range.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60copy_file_range%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: copy_file_range" (https://github.com/rrnewton/hermit/issues/342), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/342.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=copy_file_range -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(326, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
copy_file_range(-1, 0xffffffffffffffff, -1, 0xffffffffffffffff, 18446744073709551615, 4294967295) = -1 EBADF (Bad file descriptor)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/342#issuecomment-5872957345
