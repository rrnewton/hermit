---
schema: syscall-pages/v1
syscall: getxattr
nr_x86_64: 191
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1950
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getxattr&body=Discussion+for+the+%60getxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getxattr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`getxattr(path, name, value, size)` reads the value of one extended attribute of the object named by `path`, following symlinks, or reports its size when `size` is 0. Hermit forwards it to the host kernel unchanged: the attribute store is the container filesystem, which Hermit treats as fixed input, and every xattr writer is passed through too.

## Linux semantics

The kernel copies `name`, resolves `path` (following a symlink in the last component, unlike `lgetxattr`), checks permission for the name's namespace, and asks the filesystem or security module for the value. The result is the value's length, with the bytes copied into `value`, or an errno. Some namespaces are synthesized rather than stored: `security.*` values may come from the LSM (for example `security.selinux` on an SELinux host), and `system.posix_acl_*` values are encoded from the inode's ACL. The call has no side effect: on the host probe the inode's ctime did not change. Namespace support depends on the filesystem type and mount options.

### Flags and options

- F1: `size = 0`: returns the current value's length and writes nothing (`value` may be NULL). This is the size probe programs use before allocating.
- F2: `size` at least the value's length: copies the value and returns its length. A `size` above 65536 (`XATTR_SIZE_MAX`) is clamped, so a 1 MiB buffer works (host probe).
- F3: `0 < size <` value length: `ERANGE`
- F4: an attribute with an empty value: returns 0
- F5: the attribute is absent: `ENODATA`. Also `ENODATA` for a `user.*` name on an inode that is neither a regular file nor a directory, and for `trusted.*` without `CAP_SYS_ADMIN` in the initial user namespace (host probe, unprivileged).
- F6: namespace from the name prefix: `user.` (stored), `trusted.` (stored, privileged), `security.` (LSM label or stored value, for example `security.capability` on a binary), `system.posix_acl_access`/`system.posix_acl_default` (the ACL in binary form, `ENODATA` if the inode has none). No recognised prefix or an unknown `system.` name: `EOPNOTSUPP`. Missing filesystem support is F12.
- F7: `name` empty or longer than 255 bytes: `ERANGE`; a bare prefix such as `user.`: `EINVAL` (host probe)
- F8: path resolution follows symlinks in every component: a dangling symlink or missing path gives `ENOENT`; `ENOTDIR`; `ENAMETOOLONG`; `ELOOP`; no search permission on a prefix `EACCES`
- F9: permission on the inode: reading a `user.*` attribute needs read access, so a mode-000 file gives `EACCES` even to its owner (host probe)
- F10: `path`, `name`, or `value` with a nonzero `size` points outside the address space: `EFAULT` (a NULL `value` with `size = 64` gave `EFAULT` in the host probe)
- F11: no side effects: no mutation of the value, the inode or its ctime (host probe), and no inotify event
- F12: host-config failure: a filesystem without xattr support for the namespace, or without ACL support for `system.posix_acl_*`: `EOPNOTSUPP`.

## Determinism analysis

getxattr returns filesystem metadata bytes and errors from the host object and namespace. Within one Hermit run on one host, the result is a function of the filesystem state, the path, the name, the credentials and the filesystem's xattr support. Threads run one at a time under Hermit, so a size probe (F1) followed by a read (F2) sees no guest writer in between, and a guest thread racing `rename`, `unlink` or `setxattr` against the read gets a fixed order. The call is `conditional`: values, support, permissions and security labels can differ across hosts and filesystems, and host processes can change them between runs.

### Nondeterminism sources

- Attributes that come with the filesystem image rather than from the guest: LSM labels such as `security.selinux` (present only on hosts with that LSM), `security.capability` on binaries, ACLs, and attributes left by earlier runs in a non-scratch directory.
- Filesystem type and mount options, which decide namespace and ACL support (F6).
- Credentials, including the privilege of the guest's root inside Hermit's user namespace (F5, F9).
- External changes to the path or the attribute by host processes.

### Cross-host variation

- Host security module: labels such as `security.selinux` exist only on hosts running that LSM, and their values come from its policy (F5, F6).
- Filesystem type, kernel version and mount options behind the guest's paths: whether the filesystem supports the attribute's namespace and ACLs decides `EOPNOTSUPP` (F12); for example tmpfs gained `user.*` support only in Linux 6.6.

## Hermit design

Target policy: `passthrough`, changed from the rendered `investigate`. The code already does this at hermit f4055eb9cae4. `getxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:739`, under the comment at `detcore/src/syscall_classification.rs:737` ("getxattr/lgetxattr/removexattr/setxattr are deterministic for stable objects and do not introduce asynchronous state or new kernel objects"), in the block that opens at `detcore/src/syscall_classification.rs:690` and ends at `detcore/src/syscall_classification.rs:804`. The census lists it in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1711`, and `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists it at `detcore/src/syscall_classification.rs:2066` and asserts `PassThrough` at `detcore/src/syscall_classification.rs:2094`. The generic arm at `detcore/src/lib.rs:2843` sends it to `Detcore::passthrough` (`detcore/src/lib.rs:367`), which forwards it unchanged.

The 2026-08-07 strategy draft (kept under Additional information) proposed serving xattrs from a deterministic metadata store keyed by object identity, or snapshotting host values and validating them on replay. This body does not adopt it, for three reasons:

- Every xattr writer (`setxattr`, `lsetxattr`, `fsetxattr`, `removexattr` and its variants, between `detcore/src/syscall_classification.rs:739` and `detcore/src/syscall_classification.rs:793`) is passed through, and so are `chmod`-family calls that change what `system.posix_acl_access` reports. A store for reads alone would stop reading back what the guest wrote. A model would have to cover all twelve xattr syscalls, which is a filesystem model Hermit does not have.
- Within a run on one host, nothing in the result varies from run to run. The attributes that differ between hosts (LSM labels, ACLs, capabilities on binaries) are part of the filesystem image, the same input class as file contents, which Hermit also passes through.
- The legacy draft's own acceptance text ("Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state") treats modelling as future new syscall support, not as current policy.

Run mode subscribes every syscall (`Subscription::all_syscalls()` at `detcore/src/lib.rs:1094`). Code facts show `subscribed_record_replay: false`: under `--passthru-opt`, which the comment at `detcore/src/lib.rs:3140` says `hermit record` and `hermit replay` turn on, `getxattr` never reaches Detcore and is not recorded. A replay reads the replay host's attributes, so record/replay fidelity for this call depends on the replay filesystem matching the recording one. The legacy draft's "snapshot and validate on replay" idea addresses exactly that gap; it is listed as an open question below rather than adopted.

Existing evidence: `tests/c/syscall_file_metadata.c:171` calls glibc `getxattr(path, "user.hermit", buf, 32)` after a `setxattr`. When the `setxattr` succeeded, it requires the exact 9-byte value (`tests/c/syscall_file_metadata.c:172`, checked at `tests/c/syscall_file_metadata.c:57`); otherwise it accepts `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` or `EPERM` (`tests/c/syscall_file_metadata.c:37`). It runs on ptrace under `run --strict --verify` in `hermit-cli/tests/syscall_file_metadata.rs:51`, whose trace assertion list (`hermit-cli/tests/syscall_file_metadata.rs:91`) does not name `getxattr`, and in the cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). verification on the default backend stays untested: no test names `SYS_getxattr` and the success path is optional.

## Recommended tests

- T1 (covers F1, F2, F3, F4): after `setxattr(path, "user.k", "vv", 2, 0)` and an empty `user.e`, call raw `syscall(SYS_getxattr, ...)` with `size = 0` and a NULL buffer (expect 2), `size = 2` (expect 2 and the bytes `vv`), `size = 1` (expect `ERANGE`), `size = 1 MiB` (expect 2), and read `user.e` (expect 0).
  Existing partial coverage of the exact-buffer case through glibc: `tests/c/syscall_file_metadata.c:171`, which is skipped when `setxattr` failed.
- T2 (covers F5): assert `ENODATA` for a missing `user.*` name on a regular file and on a directory, for `user.k` read through `getxattr` on a FIFO, and record what the ptrace guest's root gets for `trusted.k`.
- T3 (covers F6): assert `EOPNOTSUPP` for `noprefix` and `system.bogus` and for a file under `/proc`; assert `ENODATA` for `system.posix_acl_access` on a file without an ACL. On a fixture filesystem with ACL support, set an ACL and assert the exact bytes returned.
- T4 (covers F7, F10): assert `ERANGE` for an empty and a 256-byte name, `EINVAL` for `user.`, and `EFAULT` for `path`, `name`, or `value` set to `(void *)1` with `size = 64`.
- T5 (covers F8): through a symlink to a file with `user.k` set, assert the target's value is returned; assert `ENOENT` for a dangling symlink and a missing path, plus `ENOTDIR`, `ELOOP` and `ENAMETOOLONG`.
- T6 (covers F9): assert `EACCES` for `user.k` on a mode-000 file owned by the guest.
- T7 (covers F11): assert that `st_ctime` and the value are unchanged after a `getxattr`, and that an inotify watch sees no event.
- T8 (covers F1, F2, F8): one guest thread alternates `setxattr` values of different lengths and `rename`s the file while another does a size probe followed by a read. Under `hermit run --verify` on ptrace the sequence of results (`ERANGE`, `ENOENT` or value) must match across runs.
- T9 (covers none): run T1 under `hermit record` and `hermit replay` on ptrace and assert the replayed output matches the recording, so the record/replay gap above has a test once it is decided.
- U1 (covers F12; host-config): which namespaces and ACLs the host filesystem supports is host configuration that CI cannot choose, so CI cannot produce this `EOPNOTSUPP` on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:739`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. getxattr is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1950: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional (confirmed); target_policy investigate -> passthrough (forwarded at detcore/src/lib.rs:2843 and every xattr writer is passed through, so a read-only store would break read-after-write; the 2026-08-07 model draft is kept as history); listed 11 flag behaviours and 9 tests; record/replay fidelity left as an open question; ptrace_verification stays untested; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM labels, filesystem namespace and ACL support); split filesystem-support EOPNOTSUPP out of F6 into new F12 with U1 host-config; resolved the open question on hiding host LSM labels

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getxattr&body=Discussion+for+the+%60getxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: getxattr" (https://github.com/rrnewton/hermit/issues/1950), legacy variant C. Comment history stays on that issue.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- Record/replay: `getxattr` is not in the `--passthru-opt` subscription, so `hermit replay` reads the replay host's attributes instead of the recorded ones. Should record/replay capture xattr reads (the 2026-08-07 "snapshot and validate" idea), or is a matching replay filesystem part of the replay contract, as it is for file contents? Owner decision; it does not change the run-mode policy.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(191, ...)` through Python `ctypes` on this host (Linux 7.1.3, unprivileged uid, btrfs, no SELinux). Not run under Hermit. `user.k` held the 2-byte value `vv`; `user.e` held an empty value.

```text
getxattr user.k probe size 0                -> 2
getxattr user.k exact 2                     -> 2 (value b'vv')
getxattr user.k short 1                     -> -1 ERANGE
getxattr user.k big 64                      -> 2
getxattr user.k size 1 MiB                  -> 2
getxattr user.e empty value                 -> 0
getxattr missing attr                       -> -1 ENODATA
getxattr on directory, missing attr         -> -1 ENODATA
getxattr missing path                       -> -1 ENOENT
getxattr ENOTDIR                            -> -1 ENOTDIR
getxattr no prefix                          -> -1 ENOTSUP
getxattr name empty                         -> -1 ERANGE
getxattr name 256                           -> -1 ERANGE
getxattr 'user.'                            -> -1 EINVAL
getxattr bad buf ptr                        -> -1 EFAULT
getxattr NULL value, size 64                -> -1 EFAULT
getxattr via symlink (follows)              -> 1
getxattr dangling symlink                   -> -1 ENOENT
getxattr security.selinux                   -> -1 ENODATA
getxattr system.posix_acl_access (no ACL)   -> -1 ENODATA
getxattr trusted.k (unprivileged)           -> -1 ENODATA
getxattr mode 000 user.k (owner)            -> -1 EACCES
getxattr: ctime changed                     -> False
```

### Legacy strategy draft (2026-08-07)

Hermit design draft: "Serve xattrs from deterministic filesystem metadata keyed by resolved object identity, with a defined namespace policy. If host xattrs are imported, snapshot values and provenance and validate the same object during replay." Not adopted; see Hermit design.

Legacy required tests, folded into T1-T9 above: user attributes, missing attributes, missing paths, denied namespaces, empty values and buffer-size probing (T1-T6); exact, short and large buffers and invalid pointers (T1, T4); racing rename/unlink and xattr mutation under scheduler control (T8); strict repeats and record/replay (T8, T9). The legacy "across filesystems and supporting backends" comparison is dropped: tracking is ptrace only (owner Q3), and filesystem differences are recorded per test fixture instead.

Legacy acceptance text: "Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy."

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

getxattr is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
