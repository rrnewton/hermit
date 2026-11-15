---
schema: syscall-pages/v1
syscall: access
nr_x86_64: 21
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1935
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+access&body=Discussion+for+the+%60access%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccess.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60access%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`access(pathname, mode)` asks whether the calling process could read, write or execute `pathname`, or whether it exists, using the process's real user and group ids. Hermit forwards it to the host kernel unchanged, as it does `faccessat` and `faccessat2`; the answer repeats when the guest's container filesystem and credentials are fixed.

## Linux semantics

The kernel resolves `pathname` against the calling process's working directory, following symlinks in every component including the last, and then runs the normal permission check with the real uid and gid substituted for the effective ones. It returns 0 if every requested permission is granted and a negative errno otherwise; it writes nothing and changes nothing. Root (a process with `CAP_DAC_OVERRIDE` in a user namespace that maps the file's owner) passes read and write checks regardless of mode bits, but passes `X_OK` only if some execute bit is set. Write checks also fail on read-only mounts and on executables being run. Errors are checked in path-walk order, so the errno is a function of the path, the filesystem state, the real credentials and the mount flags.

### Flags and options

- F1: `mode` values: `F_OK` (existence only), and any combination of `R_OK`, `W_OK` and `X_OK`
- F2: bits outside `F_OK|R_OK|W_OK|X_OK` in `mode`: `EINVAL`
- F3: the check uses the real uid and gid, not the effective ones, and the real uid's capabilities; a real-root process passes `R_OK` and `W_OK` on any file whose owner its namespace maps, and passes `X_OK` only if at least one execute bit is set
- F4: inside a user namespace, root has no override on files owned by an unmapped uid; those are checked against the "other" bits
- F5: symlinks are followed in every component, including the last
- F6: a component does not exist: `ENOENT`; an intermediate component is not a directory: `ENOTDIR`
- F7: a requested permission is denied, or search permission is denied on a directory in the path: `EACCES`
- F8: `W_OK` on a file on a read-only mount: `EROFS`
- F9: `W_OK` on a regular file currently being executed: `ETXTBSY`
- F10: a component longer than `NAME_MAX` or a path longer than `PATH_MAX`: `ENAMETOOLONG`; too many symlinks: `ELOOP`; `pathname` outside the address space: `EFAULT`
- F11: `X_OK` on a file on a `noexec` mount: `EACCES`
- F12: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace

## Determinism analysis

The result is a function of the filesystem state along the path, the process's real credentials and capabilities, and the mount flags. Nothing in the call depends on timing. Under Hermit, threads and processes run one at a time, so a racing rename or chmod by another guest thread lands in a fixed order relative to the query. The guest's identity is fixed: Detcore presents a constant virtual root (`getuid` and friends are virtualized to 0, per the comment at `detcore/src/syscall_classification.rs:638`), and on ptrace the guest runs in a user namespace that maps the real uid to 0, so the kernel's real-uid check agrees with that identity. The call repeats exactly when the guest-visible filesystem starts in the same state and nothing outside the container changes it, which is why `linux_determinism` is conditional.

### Nondeterminism sources

- Changes to the filesystem made outside the container.
- Which host uids own the files the guest can see: files owned by uids the namespace does not map fall under F4, so the answer depends on the host image.
- Host mount flags behind F8 and F11.
- Running without a user namespace (`--no-namespace`), where the kernel checks the host's real uid while Detcore reports 0; that mode is reduced fidelity (see Hermit design).

### Cross-host variation

- Host mount flags on the paths the guest can see: a read-only mount gives `EROFS` for `W_OK` (F8) and a `noexec` mount gives `EACCES` for `X_OK` (F11). The same guest and image get success on a host whose mounts lack these flags.

## Hermit design

Target policy: `passthrough`. The code already does this. `access` is classified `PassThrough` at `detcore/src/syscall_classification.rs:695`, the first entry of the pass-through block that starts at `detcore/src/syscall_classification.rs:690`, whose comment at `detcore/src/syscall_classification.rs:691` gives the condition ("conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions"); it is in the census at `detcore/src/syscall_classification.rs:1680`. The blanket arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. Its virtual cost is the metadata class at `detcore/src/syscall_time.rs:55`.

The two newer forms of the same query take the same route: `faccessat2` at `detcore/src/syscall_classification.rs:722`, under the comment at `detcore/src/syscall_classification.rs:719` ("deterministic process-state transitions or checks given a fixed namespace, credential set, and filesystem image"), and `faccessat` at `detcore/src/syscall_classification.rs:771`. Treating `access` differently from them would make the answer depend on which wrapper libc chose.

The 2026-08-07 strategy draft (kept under Additional information) asked for `access` to be modelled through "the same deterministic pathname, credential, and metadata authority used by other filesystem queries", and otherwise refused in strict mode. That is rejected:

- Hermit has no pathname or metadata authority. Path queries (`stat`, `faccessat`, `getcwd`) are forwarded to the kernel, and the container's mount namespace is the state. A model of `access` alone would disagree with the `open` that follows it.
- The credential half is already fixed: the guest's ids are a constant virtual root, and on ptrace the kernel's real uid is that root. Once Detcore models `setuid` and friends (owner decision 4) it keeps the record without enforcement: after a modelled `setuid(1000)`, `getuid` reads 1000 but the kernel still checks the namespace root, so `access` keeps answering as root.
- Refusing `access` would break nearly every shell and build tool, for no gain in repeatability on ptrace.

Code facts show `subscribed_record_replay: false`: `access` is not in the `--passthru-opt` list near `detcore/src/lib.rs:1214`. `hermit record` uses its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places outside the exported tree. The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) requires path queries to replay from the recording rather than from the replay chroot, whose doc comment at `hermit-cli/tests/record_replay.rs:2165` notes the chroot lacks `/etc/passwd`; its guest asserts `syscall(SYS_access, "/etc/passwd", R_OK)` returns 0 at `tests/c/record_replay_path_queries.c:197`. That is the single test counted in `tests_naming`, and it is record-mode evidence, not a ptrace `run --verify` cell.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, the kernel checks the launching host user's real uid and capabilities while Detcore reports uid 0, so `access` can return `EACCES` on a root-owned `0600` file the guest believes it can read.

## Recommended tests

- T1 (covers F1, F3, F12): under `hermit run --verify` on ptrace, `chdir` into a fresh directory, create files with modes `0000`, `0400`, `0600`, `0700` and `0644`, and call raw `syscall(SYS_access, ...)` with relative paths for `F_OK`, each single bit and `R_OK|W_OK|X_OK`. Assert the virtual root passes `R_OK`/`W_OK` on all of them and `X_OK` only on `0700`.
  Existing partial coverage via glibc `access()`: `tests/c/path_file_ops.c:97` (`F_OK` and `R_OK|W_OK` on a created file) and `tests/c/path_file_ops.c:106` (`ENOENT` for a missing path); cell `c-programs/path-file-ops`, `tests/e2e/manifests/c-programs.yaml:12961`, ptrace verify in CI.
- T2 (covers F2): assert `EINVAL` for `mode = 0x10` and for `mode = -1`.
- T3 (covers F4, F7): assert `EACCES` for `R_OK` on a `0600` file owned by an unmapped host uid that the guest can see, and for `F_OK` through a directory it cannot search. Record the paths used, which depend on the container image.
- T4 (covers F5, F6): assert a symlink to a `0700` file answers like the target, a dangling symlink gives `ENOENT`, a missing intermediate directory gives `ENOENT`, and a regular file used as a directory gives `ENOTDIR`.
  Existing partial coverage via glibc `access()`: `tests/c/errno_path_identity.c:91` (`access("/nonexistent-hermit-fixture", R_OK)` reports `ENOENT`; cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`, ptrace verify in CI).
- T5 (covers none): where the guest can see a read-only mount and a `noexec` mount, assert `EROFS` for `W_OK` and `EACCES` for `X_OK` on a file there, and record the mounts. Those errors come from the host's mount flags (host-config, U1), so this test documents them but does not cover F8 or F11.
- T6 (covers F9): copy a small binary, run it in a child that blocks, assert `W_OK` on the copy gives `ETXTBSY`, then reap the child. The test must state whether the host kernel still reports `ETXTBSY` for this case.
- T7 (covers F10): assert `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, `ELOOP` for a symlink loop, and `EFAULT` for `pathname = (char *)1`.
- U1 (covers F8, F11; host-config): the host's mount flags decide whether `EROFS` (F8) and the `noexec` `EACCES` (F11) occur, so CI cannot produce them on every host. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:695`, `detcore/src/lib.rs:2843`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. access is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1935: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: kept conditional, target_policy investigate -> passthrough (classified PassThrough at detcore/src/syscall_classification.rs:695 like faccessat and faccessat2; forwarded at detcore/src/lib.rs:2843; the legacy path-authority draft is rejected because Hermit has none); listed 12 flag behaviours and 7 tests; open question on the host real uid under --no-namespace; no coverage token, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (host mount flags: read-only and noexec); U1 host-config covers F8 and F11, T5 now covers none; no open question resolved
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (host real uid checked under --no-namespace and namespace-less backends); noted that a modelled setuid leaves the kernel checking root; target_policy unchanged (passthrough); no new F/T ids; removed the --no-namespace F3 open question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `access`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+access&body=Discussion+for+the+%60access%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Faccess.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60access%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: access" (https://github.com/rrnewton/hermit/issues/1935), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- F9: whether current host kernels still return `ETXTBSY` for `access(W_OK)` on a running executable is not confirmed here; T6 should establish it before it is required.

### Legacy strategy draft (2026-08-07)

Model access through the same deterministic pathname, credential, and metadata authority used by other filesystem queries. If full emulation is not yet available, record the resolved identity, return value, and errno with replay-side validation; a fixed deterministic refusal is preferable to an unqualified host query in strict mode.

Legacy determinism text: access answers a pathname permission query using host filesystem namespace, credentials, mount state, metadata, and race timing. Those inputs can vary across runs and backends, including error precedence and path-race outcomes.

Legacy required tests (2026-08-07):

- Exercise success, missing-path, denied-permission, symlink, and concurrent-rename cases.
- Compare repeated strict runs and record/replay for identical return values and errno.
- Cover ptrace and each backend that claims support, including dir/mount namespace differences.
- Bracket the gate with a negative fixture proving an unmodeled host-dependent result is refused and a positive fixture proving the deterministic path executes.

Legacy acceptance text (2026-08-07): Move this row out of PASSTHROUGH only when the implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

access is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
