---
schema: syscall-pages/v1
syscall: fgetxattr
nr_x86_64: 193
linux_determinism: deterministic
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/354
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fgetxattr&body=Discussion+for+the+%60fgetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffgetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fgetxattr%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 0
---

## Summary

`fgetxattr(fd, name, value, size)` reads the value of one extended attribute of the file open on `fd`. Hermit forwards it to the host kernel unchanged. The value comes from the guest's container filesystem, and Detcore keeps no extended-attribute state of its own.

## Linux semantics

The kernel resolves `fd` to its open file, copies `name` from guest memory and passes the request to the filesystem's handler for the name's namespace prefix. The access mode of `fd` does not matter: a read-only descriptor can read attributes, because the permission check is made against the inode. The handler, or the security module for `security.*` names, produces the value bytes. The kernel copies at most `size` bytes to `value` and returns the value length. The call changes nothing in the file. Values are opaque bytes, except where the kernel builds them: POSIX ACLs and file capabilities are converted into the caller's user namespace on the way out.

### Flags and options

- F1: `user.*` names: the caller needs read permission on the inode. On an inode that is neither a regular file nor a directory (a FIFO or device opened by `fd`), `user.*` reads give `ENODATA`.
- F2: `trusted.*` names are visible only with `CAP_SYS_ADMIN` in the initial user namespace. Without it the kernel answers `ENODATA`, as if the attribute did not exist. Hermit's guest root inside a user namespace is in this case.
- F3: `security.*` names: a security module may supply the value (for example `security.selinux`), and `security.capability` is converted between file-capability formats according to the caller's user namespace.
- F4: `system.posix_acl_access` and `system.posix_acl_default`: the value is built from the inode's ACL, with uids and gids mapped into the caller's user namespace. `ENODATA` when the inode has no such ACL. Missing ACL support is F12.
- F5: a name with no known namespace prefix: `EOPNOTSUPP`
- F6: `size == 0`: the call returns the current value length and does not write `value`
- F7: `size` smaller than the value: `ERANGE`. A `size` above `XATTR_SIZE_MAX` (65536) is clamped to it.
- F8: an empty name, or a name longer than `XATTR_NAME_MAX` (255 bytes): `ERANGE`
- F9: the attribute does not exist: `ENODATA`. Missing filesystem support is F12.
- F10: `fd` is not open, or was opened with `O_PATH`: `EBADF` (host probe, kernel 7.1.3, under Additional information)
- F11: `name` or `value` points outside the address space: `EFAULT`
- F12: host-config failure: a filesystem without extended-attribute support, or without ACL support for `system.posix_acl_*`: `EOPNOTSUPP`.

## Determinism analysis

The result is a function of the attribute's stored bytes, the caller's credentials and user namespace, the filesystem type and the host's security-module policy. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so a guest thread that sets the attribute while another reads it is ordered the same way in every run. The call repeats exactly when the guest-visible file starts with the same attributes and nothing outside the container changes them.

### Nondeterminism sources

- Changes to extended attributes made outside the container. Hermit does not try to make a changing host filesystem deterministic.
- Host configuration behind F2 to F4 and F9: the active security module and its policy, ACL and xattr support on the backing filesystem, and the user-namespace mapping. These are fixed for one host but differ between hosts, so a recording replayed on another host can see different values or errors.

### Cross-host variation

- Host security module: labels such as `security.selinux` exist only on hosts running that LSM, and their values come from its policy (F3).
- Filesystem type, kernel version and mount options behind the guest's paths: whether the filesystem supports the attribute's namespace and ACLs decides `EOPNOTSUPP` (F12); for example tmpfs gained `user.*` support only in Linux 6.6.

## Hermit design

Target policy: `passthrough`. The code already does this. `fgetxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:777`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The block comment at `detcore/src/syscall_classification.rs:767` gives the condition: "Stable guest-owned metadata ... are repeatable in Hermit's fixed mount namespace and filesystem image", and the line at `detcore/src/syscall_classification.rs:769` leaves that boundary open for human review. The generic arm at `detcore/src/lib.rs:2843` forwards the call through `passthrough` at `detcore/src/lib.rs:367`, which calls `record_or_replay_preserving_tool_errors` and nothing else. The census unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins the classification at `detcore/src/syscall_classification.rs:2054`; it checks the table, not the behaviour.

Why passthrough, not `model` or `emulate`:

- Hermit has no virtual filesystem. The attribute bytes live in the container's filesystem, and only the kernel applies F1 to F4 the same way the guest's `setxattr`, `listxattr` and `stat` calls see them.
- The value is not timing-dependent. Its inputs are filesystem state and host configuration, which Hermit treats as fixed in run mode.
- The only kernel-generated content (F3 and F4) is converted against the guest's own user namespace, which Hermit holds fixed for the run.

`detcore/src/syscalls/helpers.rs:857` maps `Syscall::Fgetxattr` to its fd in `get_fd`, so code that needs a syscall's descriptor can find it; the call still takes the pass-through arm. Code facts show `subscribed_record_replay: false`, so in `--passthru-opt` mode the call never reaches Detcore. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module that is not in the exported tree, so this body does not describe how record captures the returned value.

The open gap is test evidence, not code. `tests_naming` is 0: no test names `SYS_fgetxattr`. The one test that runs the call does not check any F item strictly (see Recommended tests).

## Recommended tests

- T1 (covers F1, F6, F7, F9): under `hermit run --verify` on ptrace, call raw `syscall(SYS_fgetxattr, ...)` on a regular file in the guest's `/tmp` after setting `user.a` to a 10-byte value. Assert the length from `size == 0`, `ERANGE` for `size = 4`, the exact bytes for a large buffer, and `ENODATA` for a missing name. Repeat on a read-only descriptor and on a FIFO's descriptor (expect `ENODATA` for `user.*`).
  The test must fail, not skip, if the backing filesystem rejects `user.*` attributes.
  Existing partial coverage: `tests/c/syscall_file_metadata.c:185` calls the glibc wrapper, run by `hermit-cli/tests/syscall_file_metadata.rs:51` (whose trace check names `fgetxattr` at `hermit-cli/tests/syscall_file_metadata.rs:94`) and by manifest cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). It accepts `EACCES`, `ENODATA`, `ENOTSUP`, `EOPNOTSUPP` and `EPERM` as success (`tests/c/syscall_file_metadata.c:37`), and compares the value only if `fsetxattr` succeeded (`tests/c/syscall_file_metadata.c:183`), so it passes even when every call fails.
- T2 (covers F2, F3): read `trusted.x` (expect `ENODATA`), then write a version-2 `security.capability` value with `fsetxattr` and read it back. Assert that the returned capability value is in the format for the guest's user namespace, and that the result matches between the two `--verify` runs.
- T3 (covers F4): set an access ACL through `fsetxattr("system.posix_acl_access")` and read it back. Assert the exact bytes and the ids inside them. Assert `ENODATA` for `system.posix_acl_default` on a regular file.
- T4 (covers F5, F8): assert `EOPNOTSUPP` for `bogus.a`, and `ERANGE` for `""` and for a 256-byte name.
- T5 (covers F10, F11): assert `EBADF` for a closed descriptor and for an `O_PATH` descriptor, and `EFAULT` for `name = (char *)1` and for `value = (char *)1` with a non-zero `size`.
- U1 (covers F12; host-config): which namespaces and ACLs the host filesystem supports is host configuration that CI cannot choose, so CI cannot produce this `EOPNOTSUPP` on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:777`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/354#issuecomment-5872984561
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/354: linux_determinism conditional (from **Classification:**); target_policy from legacy PASS-THRU; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; fgetxattr records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/354 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional -> deterministic (the value is a function of stored attributes, credentials and host policy, with no timing input; same footing as removexattr); kept passthrough (forwarded at detcore/src/lib.rs:2843, classified at detcore/src/syscall_classification.rs:777); 11 flag items, 5 tests; no coverage token because the only test tolerates every expected errno, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM labels, filesystem xattr and ACL support); split filesystem-support EOPNOTSUPP out of F4 and F9 into new F12 with U1 host-config; no open questions section

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `fgetxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+fgetxattr&body=Discussion+for+the+%60fgetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ffgetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60fgetxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: fgetxattr" (https://github.com/rrnewton/hermit/issues/354), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/354.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=fgetxattr -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(193, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
fgetxattr(-1, 0xffffffffffffffff, 0xffffffffffffffff, 18446744073709551615) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/354#issuecomment-5872984561

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(193, ...)` through Python `ctypes` on the measurement host (kernel 7.1.3, btrfs, not under Hermit), against a file holding `user.b = "v"` and `user.big = "0123456789"`: `size == 0` returned 1 for `user.b`; `size = 4` for `user.big` gave `ERANGE`; a missing name gave `ENODATA`; an `O_PATH` descriptor gave `EBADF`. On the same host, `removexattr` with `bogus.a` gave `ENOTSUP` and with an empty or 305-byte name gave `ERANGE`, The name check is shared by all the xattr calls, so `fgetxattr` is expected to answer F5 and F8 the same way; that part is an inference, not probed directly.
