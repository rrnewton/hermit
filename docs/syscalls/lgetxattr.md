---
schema: syscall-pages/v1
syscall: lgetxattr
nr_x86_64: 192
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1951
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lgetxattr&body=Discussion+for+the+%60lgetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flgetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lgetxattr%60%2C+or+in+what+the+page+says%3F%0A
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

`lgetxattr(path, name, value, size)` is `getxattr` without following a symlink in the last path component: when `path` names a symlink, it reads the link's own attribute. Hermit forwards it to the host kernel unchanged, with the container filesystem as the attribute store; every xattr writer is passed through too.

## Linux semantics

Identical to `getxattr(2)` except for path resolution: symlinks in earlier components are followed, the last component is not. On a symlink, `user.*` names always give `ENODATA`, because Linux allows the `user.` namespace only on regular files and directories. This holds for a dangling symlink too, which `getxattr` would report as `ENOENT` (host probe). A symlink can carry `security.*` labels and privileged `trusted.*` values. When `path` does not end in a symlink, the call behaves exactly like `getxattr`. The result is the value's length with the bytes copied into `value`, or an errno. The call has no side effect.

### Flags and options

- F1: `size = 0`: returns the current value's length and writes nothing (`value` may be NULL)
- F2: `size` at least the value's length: copies the value and returns its length; a `size` above 65536 is clamped
- F3: `0 < size <` value length: `ERANGE`
- F4: an attribute with an empty value: returns 0
- F5: the last component is a symlink: the link itself is read. A `user.*` name gives `ENODATA` whether or not the link points anywhere (host probe, including a dangling link). `security.*` returns the link's LSM label if the LSM provides one, else `ENODATA`. `trusted.*` gives `ENODATA` without `CAP_SYS_ADMIN` in the initial user namespace.
- F6: the last component is not a symlink: same result as `getxattr`, including `user.*` values on a regular file (host probe)
- F7: the attribute is absent on a non-symlink target: `ENODATA`; no recognised prefix or an unknown `system.` name: `EOPNOTSUPP`; `system.posix_acl_*` without an ACL: `ENODATA`. Missing filesystem support is F13.
- F8: `name` empty or longer than 255 bytes: `ERANGE`; a bare prefix such as `user.`: `EINVAL`
- F9: resolution of the earlier components: `ENOENT` for a missing path, `ENOTDIR`, `ENAMETOOLONG`, `ELOOP`, and `EACCES` without search permission
- F10: permission on the target inode: reading `user.*` on a regular file or directory needs read access (`EACCES`)
- F11: `path`, `name`, or `value` with a nonzero `size` points outside the address space: `EFAULT`
- F12: no side effects on the value, the inode or its ctime, and no inotify event
- F13: host-config failure: a filesystem without xattr support for the namespace, or without ACL support for `system.posix_acl_*`: `EOPNOTSUPP`.

## Determinism analysis

lgetxattr has the same host-metadata exposure as getxattr but deliberately acts on a symlink itself. Within one Hermit run on one host, the result is a function of the filesystem state, the path, the name, the credentials and the filesystem's xattr support. Threads run one at a time under Hermit, so a guest thread replacing the link or renaming it while another reads gets a fixed order. Collapsing the symlink distinction, for example by resolving the link before the call, would read a different object; passing the call through keeps the kernel's rule. The call is `conditional` because LSM labels, namespace support and permissions differ across hosts and filesystems.

### Nondeterminism sources

- LSM labels on symlinks (`security.selinux` and similar), present only on hosts with that LSM.
- Filesystem type and mount options, which decide namespace support (F7).
- Credentials, including the privilege of the guest's root inside Hermit's user namespace (F5 `trusted.*`, F10).
- External changes to the link or its target by host processes.

### Cross-host variation

- Host security module: labels such as `security.selinux` exist only on hosts running that LSM, and their values come from its policy (F5, F6).
- Filesystem type, kernel version and mount options behind the guest's paths: whether the filesystem supports the attribute's namespace and ACLs decides `EOPNOTSUPP` (F13); for example tmpfs gained `user.*` support only in Linux 6.6.

## Hermit design

Target policy: `passthrough`, changed from the rendered `investigate`. The code already does this at hermit f4055eb9cae4. `lgetxattr` is classified `PassThrough` at `detcore/src/syscall_classification.rs:740`, under the comment at `detcore/src/syscall_classification.rs:737` ("getxattr/lgetxattr/removexattr/setxattr are deterministic for stable objects and do not introduce asynchronous state or new kernel objects"), in the block that opens at `detcore/src/syscall_classification.rs:690` and ends at `detcore/src/syscall_classification.rs:804`. The census lists it in `PASSTHROUGH_SYSCALLS` at `detcore/src/syscall_classification.rs:1712`, and `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) lists it at `detcore/src/syscall_classification.rs:2070` and asserts `PassThrough` at `detcore/src/syscall_classification.rs:2094`. The generic arm at `detcore/src/lib.rs:2843` sends it to `Detcore::passthrough` (`detcore/src/lib.rs:367`), which forwards it unchanged.

The 2026-08-07 strategy draft (kept under Additional information) proposed resolving the final component without following it and reading deterministic xattr state for that exact symlink identity, with snapshot-and-validate for imported host metadata. Passing the call through already gets the first half: the kernel resolves the last component without following it. The store half is not adopted, for the same reasons as for `getxattr`: every xattr writer, including `lsetxattr` (`detcore/src/syscall_classification.rs:793`) and `lremovexattr` (`detcore/src/syscall_classification.rs:791`), is passed through, so a read-only store would stop reading back what the guest wrote. The host-dependent values (LSM labels on links) are part of the filesystem image, like file contents.

Run mode subscribes every syscall (`Subscription::all_syscalls()` at `detcore/src/lib.rs:1094`). Code facts show `subscribed_record_replay: false`: under `--passthru-opt`, which the comment at `detcore/src/lib.rs:3140` says `hermit record` and `hermit replay` turn on, `lgetxattr` never reaches Detcore and is not recorded. A replay reads the replay host's attributes; see the open question below.

Existing evidence: `tests/c/syscall_file_metadata.c:199` calls glibc `lgetxattr(symlink_path, name, value_buffer, sizeof(value_buffer))` on a symlink, after the `lsetxattr` at `tests/c/syscall_file_metadata.c:195`, which the kernel refuses with `EPERM` for a `user.*` name on a symlink. So `must_exist` is 0, the call returns `ENODATA` (F5), and `require_xattr_value` (`tests/c/syscall_file_metadata.c:200`) accepts that through `expected_xattr_error` (`tests/c/syscall_file_metadata.c:37`). It runs on ptrace under `run --strict --verify` in `hermit-cli/tests/syscall_file_metadata.rs:51` and in the cell `c-programs/syscall-file-metadata` (`tests/e2e/manifests/c-programs.yaml:9275`). That exercises only the `user.*`-on-symlink path of F5 and requires only that it repeats. verification on the default backend stays untested: no test names `SYS_lgetxattr` or asserts a specific result.

## Recommended tests

- T1 (covers F1, F2, F3, F4, F6): on a regular file reached by a path with no trailing symlink, set `user.k = "vv"` and an empty `user.e`, then call raw `syscall(SYS_lgetxattr, ...)` with `size = 0` and NULL (expect 2), `size = 2` (expect `vv`), `size = 1` (expect `ERANGE`), a 1 MiB buffer (expect 2), and read `user.e` (expect 0).
- T2 (covers F5): on a symlink to a file that carries `user.k`, assert `ENODATA` from `lgetxattr` and the value from `getxattr`; on a dangling symlink assert `ENODATA` from `lgetxattr` and `ENOENT` from `getxattr`. Record what the ptrace guest gets for `security.selinux` and `trusted.k` on the link.
  Existing partial coverage of the `user.*` case through glibc: `tests/c/syscall_file_metadata.c:199`, which accepts the `ENODATA` without asserting it.
- T3 (covers F7): on a regular file, assert `ENODATA` for a missing `user.*` name and for `system.posix_acl_access` without an ACL, and `EOPNOTSUPP` for `noprefix`, `system.bogus` and a file under `/proc`.
- T4 (covers F8, F11): assert `ERANGE` for an empty and a 256-byte name, `EINVAL` for `user.`, and `EFAULT` for `path`, `name`, or `value` set to `(void *)1` with `size = 64`.
- T5 (covers F9): assert `ENOENT`, `ENOTDIR`, `ELOOP` (a loop in an earlier component) and `ENAMETOOLONG`; assert that a symlink in an earlier component is followed.
- T6 (covers F10): assert `EACCES` for `user.k` on a mode-000 regular file owned by the guest.
- T7 (covers F12): assert that the target's `st_ctime` and the link's `lstat` ctime are unchanged after `lgetxattr`, and that an inotify watch sees no event.
- T8 (covers F5, F6): one guest thread repeatedly replaces a path between a symlink and a regular file carrying `user.k` (`rename` over it) while another calls `lgetxattr` on the path. Under `hermit run --verify` on ptrace the sequence of results (`ENODATA` or value) must match across runs.
- U1 (covers F13; host-config): which namespaces and ACLs the host filesystem supports is host configuration that CI cannot choose, so CI cannot produce this `EOPNOTSUPP` on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:740`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. lgetxattr is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1951: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism conditional (confirmed); target_policy investigate -> passthrough (forwarded at detcore/src/lib.rs:2843, the kernel already applies the no-follow rule, and every xattr writer is passed through; the 2026-08-07 store draft is kept as history); listed 12 flag behaviours and 8 tests; record/replay fidelity left as an open question; ptrace_verification stays untested; tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host LSM labels, filesystem namespace and ACL support); split filesystem-support EOPNOTSUPP out of F7 into new F13 with U1 host-config; resolved the open question on hiding host LSM labels

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `lgetxattr`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+lgetxattr&body=Discussion+for+the+%60lgetxattr%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Flgetxattr.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60lgetxattr%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: lgetxattr" (https://github.com/rrnewton/hermit/issues/1951), legacy variant C. Comment history stays on that issue.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- Record/replay: `lgetxattr` is not in the `--passthru-opt` subscription, so `hermit replay` reads the replay host's attributes instead of the recorded ones. Should record/replay capture xattr reads, or is a matching replay filesystem part of the replay contract? Owner decision, shared with https://github.com/rrnewton/hermit/issues/1950; it does not change the run-mode policy.

### 2026-10-03 host probe (syscall-tracking-p3)

Raw `syscall(191/192, ...)` through Python `ctypes` on this host (Linux 7.1.3, unprivileged uid, btrfs, no SELinux). Not run under Hermit. `l` is a symlink to `f`; `user.vl` was set on `f` through the link with `setxattr`.

```text
getxattr  via symlink user.vl               -> 1
lgetxattr symlink user.vl                   -> -1 ENODATA
lgetxattr regular file user.vl              -> 1
lgetxattr dangling symlink user.k           -> -1 ENODATA
getxattr  dangling symlink user.k           -> -1 ENOENT
lgetxattr security.selinux on symlink       -> -1 ENODATA
```

### Legacy strategy draft (2026-08-07)

Hermit design draft: "Resolve the final component without following it and query deterministic xattr state for that exact symlink identity. Snapshot-and-validate any imported host metadata, including filesystem support and error provenance." The no-follow half is what the kernel does under passthrough; the store half is not adopted; see Hermit design.

Legacy required tests, folded into T1-T8 above: symlink versus target attributes, dangling links, missing attributes, denied namespaces and unsupported filesystems (T2, T3); size probes and short/exact buffers (T1); racing link replacement and rename under the deterministic scheduler (T8); strict-repeat outcomes (T8). The legacy "record/replay outcomes on all supporting backends" requirement is reduced to the record/replay open question above: tracking is ptrace only (owner Q3).

Legacy acceptance text: "Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy."

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

lgetxattr is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
