---
schema: syscall-pages/v1
syscall: memfd_create
nr_x86_64: 319
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+memfd_create&body=Discussion+for+the+%60memfd_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmemfd_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60memfd_create%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 4
---

## Summary

`memfd_create(2)` creates an anonymous, memory-backed file and returns a descriptor for it. The file has no path in any mounted file system. The kernel creates the file. Detcore then adds the descriptor to its own descriptor table as a `Memfd` entry. At this SHA that entry gets the wrong close-on-exec and status flags when `MFD_CLOEXEC` is passed (see Hermit design).

## Linux semantics

The call takes a name and a flag word. It creates a file of size 0 on the kernel's internal tmpfs mount, or on hugetlbfs when `MFD_HUGETLB` is given, and returns the lowest free descriptor number. The descriptor is opened read-write with `O_LARGEFILE`. The name appears only as `memfd:<name>` in `/proc/<pid>/fd` links and in `/proc/<pid>/maps`. Later calls use the descriptor like any file: `ftruncate`, `write`, `mmap`, `fcntl(F_ADD_SEALS)`. The memory is freed when the last reference goes away.

### Flags and options

- F1: `name`: a NUL-terminated string of at most 249 bytes, shown as `memfd:<name>`. A longer name gives `EINVAL`; a bad pointer gives `EFAULT`.
- F2: `MFD_CLOEXEC`: the new descriptor has `FD_CLOEXEC` set, so `execve` closes it.
- F3: `MFD_ALLOW_SEALING`: `fcntl(F_ADD_SEALS)` is allowed. Without it the file starts with `F_SEAL_SEAL` set, and adding a seal fails with `EPERM`.
- F4: `MFD_HUGETLB`, optionally with a page-size encoding (`MFD_HUGE_2MB`, `MFD_HUGE_1GB` and the other `MFD_HUGE_*` values): the file is backed by huge pages (F11 covers a page size the host does not support). Whether later faults or `fallocate` succeed depends on the host's huge page pool.
- F5: `MFD_NOEXEC_SEAL` (Linux 6.3): the file is created without execute permission and sealed with `F_SEAL_EXEC`; sealing is implied. `MFD_EXEC` (Linux 6.3) asks for an executable file. Passing both gives `EINVAL`.
- F6: neither `MFD_EXEC` nor `MFD_NOEXEC_SEAL`: the per-PID-namespace `vm.memfd_noexec` sysctl decides. 0 means executable. 1 means the call behaves as if `MFD_NOEXEC_SEAL` were given. 2 also rejects `MFD_EXEC` (F12), and how it treats a call naming neither flag has changed between kernel releases. Kernels before 6.3 have no such sysctl and create an executable file.
- F7: invalid flag words give `EINVAL`: an unknown bit, or an `MFD_HUGE_*` size without `MFD_HUGETLB`.
- F8: `EMFILE` at `RLIMIT_NOFILE`.
- F9: the returned object: the lowest free descriptor number, file size 0, status flags `O_RDWR | O_LARGEFILE`. `st_ino` and `st_dev` come from the kernel's internal mount.
- F10: host-transient errors: `ENFILE` when the host's system-wide file limit is reached, and `ENOMEM` from host memory. Both depend on what other host processes hold.
- F11: host-config error: `MFD_HUGETLB` with an `MFD_HUGE_*` page size the host CPU or kernel does not support gives `EINVAL`.
- F12: host-config errors: `MFD_EXEC` gives `EACCES` when `vm.memfd_noexec` is 2, and kernels before 6.3, which do not know `MFD_EXEC` or `MFD_NOEXEC_SEAL`, give `EINVAL` for either.

## Determinism analysis

With a fixed descriptor table, flags and name, the descriptor number, the error for a bad argument, and the file's contents and seals all repeat. Three things can differ:

- `st_ino` comes from a host-global inode counter, so it differs between runs.
- The results of `MFD_HUGETLB` (F4) depend on the host's huge page configuration.
- The default execute permission (F6) depends on the host kernel version and the `vm.memfd_noexec` sysctl.

`linux_determinism` is `conditional`: deterministic for a fixed host configuration, apart from the inode number.

### Cross-host variation

- Huge page support (CPU page sizes and the kernel's hugetlb configuration): which `MFD_HUGE_*` sizes are accepted (F4, F11).
- Sysctl `vm.memfd_noexec`: the default execute mode and seals of a file created with neither exec flag (F6), and whether `MFD_EXEC` is rejected (F12).
- Kernel version: whether `MFD_EXEC` and `MFD_NOEXEC_SEAL` exist (F5, F12), and how `vm.memfd_noexec=2` treats a call naming neither flag (F6).

## Hermit design

`target_policy` is `model`: the kernel creates the file, and Detcore tracks the descriptor in its own table so that later descriptor operations, exec and stat virtualization stay consistent. Code at hermit f4055eb9cae4:

- Dispatch: the arm at `detcore/src/lib.rs:2588` runs `handle_memfd_create` (`detcore/src/syscalls/files.rs:4317-4332`). `lib.rs:1184` subscribes the syscall.
- F1, F3, F4, F5, F6, F7, F8: `files.rs:4323` passes the call to the kernel through `record_or_replay` unchanged. The name, the flags, the error and the descriptor number are therefore the host kernel's.
- F9: `files.rs:4324-4330` registers the descriptor with `add_fd` as `FdType::Memfd`. When metadata virtualization is on, `add_fd` (`files.rs:916-950`) runs an `fstat` on the new descriptor and stores the result. If that `fstat` fails, it closes the descriptor and returns the error.
- F2, suspected defect: the flag-mapping bug is described in the Open questions below.
- Later operations on the descriptor:
  - `read` passes straight to the kernel (`files.rs:1862-1864`).
  - The seal commands go to the kernel through the default arm of `handle_fcntl` (`files.rs:3179-3185`).
  - `sendfile` treats memfd endpoints like regular files (`files.rs:2187-2188`).
  - The `/proc/<pid>/fdinfo` handling keeps the kernel's mount ID of 0 for anonymous objects such as memfd (`files.rs:1537-1540`, `detcore/src/tool_global.rs:349-353`).
- F4 and F6 pass host configuration through to the guest. `F_GET_SEALS` and the `fstat` mode bits differ between hosts with different `vm.memfd_noexec` settings, and `MFD_HUGETLB` succeeds or fails depending on the huge page setup.

## Recommended tests

- T1 (covers F1, F9): create memfds with a short name, a 249-byte name, a 250-byte name (`EINVAL`) and an unmapped name pointer (`EFAULT`). Print each descriptor number, the `/proc/self/fd` link text, the `fstat` size and inode, and the fdinfo `mnt_id`, and require two runs under `hermit run --verify` on ptrace to match.
  Existing candidate: `tests/c/remap_file_pages_memfd_enosys.c` (cell `c-programs/remap-file-pages-memfd-enosys` at `tests/e2e/manifests/c-programs.yaml:7350`, verify ci on ptrace). It creates, sizes and maps an `MFD_CLOEXEC` memfd but asserts only the `remap_file_pages` result.
- T2 (covers F2): create one memfd with `MFD_CLOEXEC` and one without, then exec a helper. In the helper, the first descriptor number must be closed (`fcntl(F_GETFD)` gives `EBADF`), and the next `open` must reuse it; the second descriptor must still be open. This targets the suspected model defect.
- T3 (covers F3): without `MFD_ALLOW_SEALING`, `F_GET_SEALS` returns `F_SEAL_SEAL` and `F_ADD_SEALS` gives `EPERM`. With it, add `F_SEAL_WRITE`, `F_SEAL_SHRINK` and `F_SEAL_GROW`; then `write`, `ftruncate` and a shared writable `mmap` all give `EPERM`.
- T4 (covers F4): `MFD_HUGETLB | MFD_HUGE_2MB` followed by `ftruncate` and a touch of the mapping. The result must match across runs on one host; the cell records which result the host gives.
- T5 (covers F5, F6): `MFD_NOEXEC_SEAL` gives `F_SEAL_EXEC` in `F_GET_SEALS`, and `fchmod(0755)` gives `EPERM`. `MFD_EXEC | MFD_NOEXEC_SEAL` gives `EINVAL`. With neither flag, print the `fstat` mode bits and the seals.
- T6 (covers F7, F8): an unknown flag bit and `MFD_HUGE_2MB` without `MFD_HUGETLB` both give `EINVAL`. After lowering `RLIMIT_NOFILE` to the current descriptor count, the call gives `EMFILE`.
- U1 (covers F10; host-transient): `ENFILE` and `ENOMEM` depend on the host's file table and memory, shared with other processes. `handle_memfd_create` passes the call to the kernel through `record_or_replay` (`detcore/src/syscalls/files.rs:4323`) and hands these errors to the guest today. F10 counts toward done only once Hermit hides them or reports them as an infrastructure error.
- U2 (covers F11, F12; host-config): the CI host's huge page sizes, kernel version and `vm.memfd_noexec` setting are fixed, so CI cannot produce every outcome. Hermit forwards the call and returns the kernel's error unchanged (`detcore/src/syscalls/files.rs:4323`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (kernel creates the file; Detcore tracks the descriptor as Memfd); linux_determinism stays conditional; 9 flag behaviours and 6 tests listed; suspected defect, found by reading the code: MFD_CLOEXEC is recorded as O_WRONLY, not close-on-exec, in Detcore's descriptor model; open question on host-dependent noexec and hugetlb results; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (huge page support, vm.memfd_noexec, kernel version); F8 narrowed to EMFILE; new F10, ENFILE and ENOMEM (host-transient U1), F11, unsupported huge page size, and F12, the MFD_EXEC EACCES and old-kernel EINVAL (host-config U2); the open question on pinning host-configuration results is resolved by decision 2

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `memfd_create`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+memfd_create&body=Discussion+for+the+%60memfd_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fmemfd_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60memfd_create%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `memfd_create`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Suspected defect, found by reading the code and not measured: `files.rs:4327` builds the modelled flags as `OFlag::from_bits_truncate((call.flags() & libc::MFD_CLOEXEC) as i32)`.
  - `MFD_CLOEXEC` is `0x1`, which as an `OFlag` is `O_WRONLY`, not `O_CLOEXEC` (`0x80000`).
  - `DetFd::new` (`detcore/src/fd.rs:204-213`) keeps only the `O_CLOEXEC` bit as the descriptor flag and stores the rest as status flags. A memfd created with `MFD_CLOEXEC` is therefore modelled as not close-on-exec, with status `O_WRONLY`.
  - `for_exec` (`detcore/src/tool_local.rs:572-582`) keeps every descriptor that is not close-on-exec, so after an exec Detcore's table still lists a memfd that the kernel has closed.
  - `F_GETFL` takes its result from the kernel (`files.rs:3097-3108`), so the guest does not see the wrong status flags there.
  - The fix looks mechanical: map `MFD_CLOEXEC` to `O_CLOEXEC`. It needs a hermit issue and T2 as a regression test.
