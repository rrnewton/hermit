---
schema: syscall-pages/v1
syscall: llistxattr
nr_x86_64: 195
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/634
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+llistxattr&body=Discussion+for+the+%60llistxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fllistxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60llistxattr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`llistxattr(path, list, size)` returns the names of all extended attributes of the file at `path` itself: a final symlink is not followed. Hermit forwards it to the host kernel unchanged. That leaks the host filesystem's enumeration order, which on tmpfs under host kernel 7.1.3 differs for each fresh file, so the target is a Detcore handler that canonicalizes the order.

## Linux semantics

The kernel resolves `path`, following symlinks in every component except the last, and asks the filesystem's `listxattr` operation to fill `list` with the attribute names. The format is the names concatenated, each terminated by a NUL byte, and the return value is the total length. The kernel imposes no order: each filesystem emits names in its own internal order, and the security module may add its own names. A filesystem without a `listxattr` operation reports only the security module's names, often an empty list. Listing needs path search permission but no read permission on the file. The call changes nothing.

### Flags and options

- F1: `size == 0`: the call returns the length the full list needs and does not write `list`
- F2: `size` smaller than the list: `ERANGE`. A `size` above `XATTR_LIST_MAX` (65536) is clamped to it, and a list longer than that limit gives `E2BIG`.
- F3: name order: filesystem-defined. Measured on the measurement host (kernel 7.1.3): btrfs returned the same order for every fresh file; tmpfs returned a different order for each fresh file with the same names set in the same sequence, and a stable order for repeated listings of one file. The installed kernel headers are consistent with this (an inference; see Additional information): in the 7.1.3 headers tmpfs-style in-memory attributes (`struct simple_xattrs`) are an `rhashtable`, whose hash folds in a random seed; in the installed 6.13 and 6.19 headers the same struct is an `rb_root`.
- F4: `user.*` names are listed without a read-permission check on the file
- F5: `trusted.*` names are listed only with `CAP_SYS_ADMIN` in the initial user namespace, so Hermit's guest root in a user namespace does not see them
- F6: `security.*` names: the security module may add names (for example `security.selinux`) even when the filesystem stores none, and a stored `security.capability` is listed
- F7: `system.posix_acl_access` and `system.posix_acl_default` are listed when the inode has the ACL
- F8: a final symlink is not followed: the names returned are the symlink's own. `user.*` names cannot be set on a symlink (`lsetxattr` gives `EPERM`), so a symlink lists only `trusted.*`, `security.*` and `system.*` names; the host probe got an empty list (0) for a symlink on btrfs. On any other file type the call behaves like `listxattr`.
- F9: path resolution errors: `ENOENT` (a missing component or empty path), `ENOTDIR`, `EACCES` (search permission), `ENAMETOOLONG`, `ELOOP`
- F10: `path` or `list` points outside the address space: `EFAULT`
- F11: a filesystem without a `listxattr` operation: only the security module's names, often a return value of 0, rather than an error

## Determinism analysis

The set of names is a function of filesystem state, credentials and host security policy, like `getxattr`. The order is not. On kernel 7.1.3 tmpfs lists names in an order derived from a per-file random hash seed (F3), so two runs that create the same file and set the same attributes in the same order list them differently. A guest that prints the list, hashes it, or stops at the first matching name diverges between the two runs of `--verify`. The total length (F1) and `ERANGE` (F2) do not depend on the order. Btrfs (measured) and the rb-tree tmpfs of older kernels (per their headers) give a fixed order for a fixed history, so the call repeats there.

### Nondeterminism sources

- Name order on filesystems with a randomized index: tmpfs on kernel 7.1.3, measured. Any guest-visible path backed by such a filesystem is affected.
- Changes to extended attributes made outside the container.
- Host configuration behind F5 to F7 and F11: the active security module, ACL and xattr support on the backing filesystem, and the user-namespace mapping. Fixed for one host, different between hosts.

### Cross-host variation

- Host security module: it may add names such as `security.selinux` to every listing, and only on hosts running that LSM (F6).
- Filesystem type behind the guest's paths: a filesystem without a `listxattr` operation returns only the security module's names (F11), and where the order of names is stable run to run (btrfs, or the rb-tree tmpfs of older kernels) it is the filesystem's own order, which can differ between filesystem types and kernel versions (F3).

## Hermit design

Target policy: `model`. The code does not do this yet. Today `llistxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:789`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`, under the block comment at `detcore/src/syscall_classification.rs:767` ("Stable guest-owned metadata ... are repeatable in Hermit's fixed mount namespace and filesystem image") whose boundary the line at `detcore/src/syscall_classification.rs:769` leaves open for review. The generic arm at `detcore/src/lib.rs:2843` forwards it through `passthrough` (`detcore/src/lib.rs:367`), which does nothing to the returned buffer. The census unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the `PassThrough` classification at `detcore/src/syscall_classification.rs:2074`; a handler must move that entry.

The stable-state assumption in that comment does not hold for F3: the filesystem state is the same in both runs and the order still differs. The fix has a direct precedent. `handle_getdents64` forwards the real call, reads the returned entries, sorts them and writes them back (`detcore/src/syscalls/files.rs:4622`), so directory order does not leak. An `llistxattr` handler should do the same: forward the call, and when it returns a positive length with `size > 0`, sort the NUL-separated names bytewise in place and return the same length. The `size == 0` query and every error pass through unchanged, because neither depends on order.

Why `model`, not `emulate`, `refuse` or `passthrough`:

- The kernel still has to produce the set of names: Hermit has no virtual filesystem and cannot know which names the security module adds.
- Refusing would break ordinary programs (`cp --preserve=xattr`, `tar --xattrs`, `rsync -X`) for a defect that a sort removes.
- Passthrough is correct only on filesystems with a fixed order, and the guest cannot choose its filesystem.

The getdents handlers sort only when `virtualize_metadata` is on (`detcore/src/syscalls/files.rs:4603`); the handler should follow the same switch. Code facts show `subscribed_record_replay: false`, so in `--passthru-opt` mode the call never reaches Detcore. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree.

## Recommended tests

- T1 (covers F3): under `hermit run --verify` on ptrace, on a tmpfs path the guest can reach, create a file, set six `user.*` names in a fixed non-alphabetical sequence, and print the raw `syscall(SYS_llistxattr, ...)` buffer. Run it on a host kernel whose tmpfs uses the hashed index; the test must fail rather than skip if no tmpfs path is reachable. Expected today: `--verify` reports a divergence. Expected after the handler: identical output, in bytewise-sorted order.
- T2 (covers F1, F2, F4): set `user.a` and `user.b`; assert the exact length from `size == 0`, `ERANGE` for `size = 3`, and the exact sorted buffer for a large `size`. Then `chmod 000` the file, drop `CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH` with `capset`, and assert that the listing is unchanged.
  Existing partial coverage: `tests/c/syscall_file_metadata.c:203` calls the glibc wrapper on a symlink, run by `hermit-cli/tests/syscall_file_metadata.rs:51` and by manifest cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). It accepts `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` and `EPERM` as success (`tests/c/syscall_file_metadata.c:37`) and checks the name only when `lsetxattr` succeeded (`tests/c/syscall_file_metadata.c:197`). `lsetxattr` of a `user.*` name on a symlink fails with `EPERM`, so the list is never checked, and a single name could not show an ordering defect anyway.
- T3 (covers F5, F6, F7): set a `trusted.x` name from outside the guest before launch (or assert the guest's attempt fails), an access ACL, and a version-2 `security.capability` value. Assert that `trusted.x` is absent, and that both `system.posix_acl_access` and `security.capability` are listed. Record which `security.*` names the host's security module adds.
- T4 (covers F8, F9, F10): call `llistxattr` on a symlink to the T2 file and assert the symlink's own list (no `user.*` names), then on the T2 file itself and assert its names. Assert `ENOENT` for a missing path and for `""`, `ENOTDIR`, `ENAMETOOLONG` for a 256-byte component, `ELOOP` for a symlink loop in the prefix, and `EFAULT` for `path = (char *)1` and for `list = (char *)1` with a non-zero `size`.
- T5 (covers F11): list a file on a guest-visible filesystem without a `listxattr` operation (for example under `/proc`), and assert the exact return value. The test must name the path it uses.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/634: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (forward, then sort the returned names bytewise as handle_getdents64 does at detcore/src/syscalls/files.rs:4622); linux_determinism conditional -> nondeterministic (host kernel 7.1.3 tmpfs lists the same names in a different order for each fresh file, measured); the code still passes the call through, so tracking_status stays todo; 11 flag items, 5 tests
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM names, filesystem listxattr support and name order); no U items; no open questions section

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `llistxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+llistxattr&body=Discussion+for+the+%60llistxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fllistxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60llistxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: llistxattr" (https://github.com/rrnewton/hermit/issues/634), legacy variant B.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.

### 2026-10-03 host probe (syscall-tracking-p3)

Run on the measurement host (kernel 7.1.3), not under Hermit, with Python `os.setxattr`/`os.listxattr` and raw `ctypes` syscalls. Six names (`user.zeta`, `user.alpha`, `user.mid`, `user.b`, `user.omega`, `user.c1`) were set in that order on a fresh file each time:

- btrfs, five fresh files: always `user.alpha, user.omega, user.zeta, user.b, user.mid, user.c1`.
- tmpfs (`/dev/shm`), eight fresh files: eight different orders, for example `user.mid, user.c1, user.alpha, user.b, user.zeta, user.omega` and `user.omega, user.c1, user.b, user.alpha, user.zeta, user.mid`. Listing one file twice gave the same order both times (three files checked).
- `listxattr` with `size == 0` returned 21 for three names `user.a`, `user.b`, `user.c` (7 bytes each); `size = 3` gave `ERANGE`. `llistxattr` on a symlink to that file returned 0. `flistxattr` returned `EBADF` for an `O_PATH` descriptor and for descriptor 999.

Kernel headers under `/usr/src/kernels/`: `struct simple_xattrs` in `include/linux/xattr.h` holds a `struct rhashtable` in 7.1.3, and a `struct rb_root` plus a lock in 6.13.2 and 6.19.2. `include/linux/rhashtable.h` in 7.1.3 documents `hash_rnd` as a "Random seed to fold into hash". That the tmpfs order comes from this seed is an inference from the headers and the measurement; the kernel source was not read.
