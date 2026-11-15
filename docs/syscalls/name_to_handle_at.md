---
schema: syscall-pages/v1
syscall: name_to_handle_at
nr_x86_64: 303
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/311
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+name_to_handle_at&body=Discussion+for+the+%60name_to_handle_at%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fname_to_handle_at.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60name_to_handle_at%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

Returns an opaque, persistent file handle (plus a mount id) for a path, for later use with `open_by_handle_at`. Hermit refuses every call with `EOPNOTSUPP` without reading the arguments or writing any output, because the handle bytes and the mount id are host filesystem identities that Detcore does not virtualize.

## Linux semantics

`name_to_handle_at(dirfd, pathname, handle, mount_id, flags)` resolves `pathname` relative to `dirfd` and fills `struct file_handle { handle_bytes; handle_type; f_handle[] }` with filesystem-defined bytes (commonly inode number plus generation) and `*mount_id` with the mount's id. The caller sets `handle_bytes` to the space it provided; the kernel writes back the size it needs. No privilege is needed to obtain a handle; using it with `open_by_handle_at` needs `CAP_DAC_READ_SEARCH`.

### Flags and options

- F1: path resolution: `pathname` absolute, relative to `AT_FDCWD`, or relative to a directory `dirfd`. A missing path gives `ENOENT`; a bad `dirfd` gives `EBADF`; a non-directory `dirfd` with a relative path gives `ENOTDIR`.
- F2: `AT_EMPTY_PATH`: with an empty `pathname`, the handle describes `dirfd` itself (which may be an `O_PATH` descriptor).
- F3: `AT_SYMLINK_FOLLOW`: follow a trailing symlink. Without it the handle describes the symlink.
- F4: `AT_HANDLE_FID` (Linux 6.5): request a handle usable only for identification (for example by fanotify), which filesystems without full export support can also produce.
- F5: `AT_HANDLE_MNT_ID_UNIQUE` (Linux 6.12): `*mount_id` receives the 64-bit unique mount id instead of the reusable 32-bit one.
- F6: `AT_HANDLE_CONNECTABLE` (Linux 6.13): request a handle that can be reconnected to a path when opened.
- F7: undefined `flags` bits, or an invalid combination: `EINVAL`.
- F8: `handle_bytes` too small: `EOVERFLOW`, with the needed size written back to `handle_bytes`. Probing with `handle_bytes = 0` is the documented way to learn the size.
- F9: `handle_bytes` above `MAX_HANDLE_SZ` (128): `EINVAL`.
- F10: a filesystem without export support (for example some FUSE or pseudo filesystems): `EOPNOTSUPP`.
- F11: bad `pathname`, `handle` or `mount_id` pointers: `EFAULT`.
- F12: success outputs: `handle_type`, the `f_handle` bytes, the updated `handle_bytes`, and `*mount_id`.

## Determinism analysis

A successful call writes host filesystem identities into guest memory. The handle bytes encode the inode number and, on many filesystems, a generation number chosen when the inode was created (randomized on ext4, for example); a file created during the run gets a new inode and generation each time. The mount id depends on the order in which the host's mount table was built. So two runs, or two hosts, return different bytes for the same path: `nondeterministic`. Only the error paths (F1, F7, F8 size probing, F9, F11) repeat.

### Nondeterminism sources

- `f_handle` bytes: host inode and generation numbers (F12).
- `*mount_id`: host mount-table position, or the unique mount id under F5.
- `handle_type` and the needed size: depend on the host filesystem type behind the path.
- F10: whether `EOPNOTSUPP` is returned depends on the host filesystem.

## Hermit design

Target policy: `refuse`. Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2595`): `Syscall::NameToHandleAt(_) => Err(Error::Errno(Errno::EOPNOTSUPP))`, with the comment `TODO-HUMAN-REVIEW(PR-899)` "Host object handles and mount IDs are outside Detcore's filesystem identity model" (`detcore/src/lib.rs:2593`-`2594`).
- Classification: listed as Determinized (`detcore/src/syscall_classification.rs:516`-`518`), checked by the unit test `filesystem_handle_export_is_determinized` (`detcore/src/syscall_classification.rs:2430`), and named in `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1344`), which `detcore/src/lib.rs:226` exposes so backends that run guest syscalls outside the Detcore dispatcher return the same fixed refusal.
- The refusal is unconditional. Detcore reads no argument and writes no output, so every call, whatever its path, flags, buffer size or pointers, returns `EOPNOTSUPP`. `EOPNOTSUPP` is an answer Linux gives itself for a filesystem without export support (F10), so a program that handles that case (for example by falling back to path-based access) keeps working.

Differences from Linux that remain:

- Every error path that Linux reports with another errno is replaced by `EOPNOTSUPP`: a missing path (F1, `ENOENT`), a bad `dirfd` (F1, `EBADF`), undefined flags (F7, `EINVAL`), a too-small buffer (F8, `EOVERFLOW` plus the size write-back), an oversized buffer (F9, `EINVAL`), and bad pointers (F11, `EFAULT`).
- F4: `AT_HANDLE_FID` is refused too, although Linux supports it on more filesystems than full export. Programs that use fanotify with file ids lose that path.
- The size probe (F8) does not report a size, so a program that probes before allocating sees `EOPNOTSUPP` on the probe.

Why `refuse` and not the alternatives: `passthrough` leaks host inode, generation and mount numbers, which is the reason this issue was filed; `model` or `emulate` would need Detcore to issue stable virtual handles and translate them back in `open_by_handle_at`, which the comment at `detcore/src/lib.rs:2593` places outside the filesystem identity model; `open_by_handle_at` is refused as well, so a modeled handle would have no consumer.

## Recommended tests

- T1 (covers F1, F10, F12): absolute paths `/`, `/tmp` and `/etc/hosts`, and the relative path `.`, all return `EOPNOTSUPP` under `hermit run --verify` on ptrace, and the handle's sentinel `handle_type` and `*mount_id` are left unchanged.
  Existing candidates: `tests/c/name_to_handle_at_eopnotsupp.c` (raw syscall on `/`, cell `c-programs/name-to-handle-at-eopnotsupp`, `tests/e2e/manifests/c-programs.yaml:3902`), `tests/c/name_to_handle_directory_eopnotsupp.c` (`/tmp`, `tests/e2e/manifests/c-programs.yaml:3960`), `tests/c/name_to_handle_regular_eopnotsupp.c` (`/etc/hosts`, `tests/e2e/manifests/c-programs.yaml:4076`) and `tests/c/name_to_handle.c` (`/` and `.`, with the sentinel check at `tests/c/name_to_handle.c:63`, cell `c-programs/name-to-handle-refusal`, `tests/e2e/manifests/c-programs.yaml:12775`). Each cell has verify `ci: true`. None checks `*mount_id`.
- T2 (covers F2): an `O_PATH` descriptor with an empty path and `AT_EMPTY_PATH` returns `EOPNOTSUPP`.
  Existing candidate: `tests/c/name_to_handle_empty_path_eopnotsupp.c` (cell at `tests/e2e/manifests/c-programs.yaml:4018`, verify `ci: true`).
- T3 (covers F3, F4, F5, F6, F7): each of `AT_SYMLINK_FOLLOW` (on a symlink), `AT_HANDLE_FID`, `AT_HANDLE_MNT_ID_UNIQUE`, `AT_HANDLE_CONNECTABLE` and an undefined flag bit returns `EOPNOTSUPP` under Hermit. Record the Linux results next to them.
- T4 (covers F8, F9): `handle_bytes = 0` and `handle_bytes = 129` both return `EOPNOTSUPP`, and `handle_bytes` is not rewritten.
- T5 (covers F1, F11): a missing path, a bad `dirfd`, and NULL `pathname`, `handle` and `mount_id` pointers all return `EOPNOTSUPP` under Hermit. This pins the documented divergence so a future change to it is deliberate.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/311#issuecomment-5872885759
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/311: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; name_to_handle_at records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/311 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (lib.rs:2595 returns EOPNOTSUPP for every call without reading arguments or writing output; listed in is_deterministically_refused_syscall); linux_determinism stays nondeterministic; 12 flag behaviours and 5 tests listed; five refusal cells exist (verify ci true) but cover only plain paths and AT_EMPTY_PATH; no coverage token yet, so tracking_status stays todo
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T2 covered by tests/c/name_to_handle_empty_path_eopnotsupp.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `name_to_handle_at`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+name_to_handle_at&body=Discussion+for+the+%60name_to_handle_at%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fname_to_handle_at.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60name_to_handle_at%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: name_to_handle_at" (https://github.com/rrnewton/hermit/issues/311), legacy variant A1.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/311.

The companion call `open_by_handle_at` has its own body; it is also refused.

### Open questions

- Should the refusal keep Linux's error precedence for arguments it can check without the host (`EINVAL` for undefined flags or `handle_bytes > 128`, `EFAULT` for bad pointers), and return `EOPNOTSUPP` only where Linux would otherwise succeed? Today every call gets `EOPNOTSUPP` (`detcore/src/lib.rs:2595`).

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) by tracing a direct `syscall()` harness and common programs with `strace -f -e trace=name_to_handle_at`, and by confirming hermit currently routes `name_to_handle_at` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs` (the harness ran end-to-end under `hermit run`, exit 0, so passthrough does not crash).

Observed strace line:
```
name_to_handle_at(AT_FDCWD, "/etc/hostname", {handle_bytes=128 => 20, handle_type=77, f_handle="\x99\x25\x03\x00\x00\x00\x00\x00\x05\x00\x00\x00\x00\x00\x00\x00\x0d\x00\x00\x00"}, [5201], 0) = 0
```

That routing claim is history: at hermit f4055eb9cae4 the call is refused with `EOPNOTSUPP` (`detcore/src/lib.rs:2595`).

---
*Autonomous syscall-classification sweep, batch 3 (ranks 101-150 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/311#issuecomment-5872885759
