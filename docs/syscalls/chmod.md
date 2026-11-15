---
schema: syscall-pages/v1
syscall: chmod
nr_x86_64: 90
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: true
related:
  - https://github.com/rrnewton/hermit/issues/1940
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chmod&body=Discussion+for+the+%60chmod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchmod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chmod%60%2C+or+in+what+the+page+says%3F%0A
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

`chmod(pathname, mode)` sets the permission and special mode bits of the file that `pathname` names, following symlinks. Hermit forwards it to the host kernel unchanged. It relies on the guest's container filesystem being private and stable, and on the fixed container credentials, and Detcore keeps no mode state of its own.

## Linux semantics

The kernel resolves `pathname`, following symlinks in every component including the last, so `chmod` on a symlink changes its target and a dangling symlink gives `ENOENT`. It then checks that the caller may change the mode: its filesystem uid must own the inode, or it must hold `CAP_FOWNER` over the inode's owner. In a user namespace the capability counts only when the inode's uid and gid are mapped into it. The new mode replaces the low twelve bits (`07777`) of the old one; it is not combined with it, and the umask does not apply. Only the S_ISGID adjustment (F2) and, where an access ACL exists, its mask entry (F4) change what is stored. On success the inode's ctime is set to the current time. The call returns 0 or a negative errno. It never reports the resulting mode, which a guest sees only through the stat family.

Manual reference: [chmod(2)](https://man7.org/linux/man-pages/man2/chmod.2.html).

### Flags and options

- F1: mode bits: `mode & 07777` replaces the permission bits (`0777`), `S_ISUID`, `S_ISGID` and `S_ISVTX`; file-type bits in `mode` are ignored. A second call fully replaces the first rather than OR-ing with it.
- F2: S_ISGID clearing: if the caller is not in the file's group (filesystem gid or supplementary groups) and lacks `CAP_FSETID` over the inode, the kernel silently drops `S_ISGID` from the new mode and still returns 0.
- F3: symlink following: every component, including the last, is followed. A symlink's own mode is never changed. A dangling last component gives `ENOENT`, and too many symlinks give `ELOOP`.
- F4: POSIX ACLs: when the file has an access ACL, the group bits of `mode` become the ACL mask entry (or the owning-group entry if there is no mask), so a later `getxattr("system.posix_acl_access")` changes too.
- F5: ownership: the caller's filesystem uid does not own the file and it lacks `CAP_FOWNER` over the inode: `EPERM`.
- F6: path errors: a component is missing or `pathname` is empty: `ENOENT`. An intermediate component is not a directory: `ENOTDIR`. Search permission is denied on a prefix: `EACCES`. A component longer than `NAME_MAX` or a path longer than `PATH_MAX`: `ENAMETOOLONG`. `pathname` outside the address space: `EFAULT`.
- F7: `EROFS`: the file is on a read-only mount in the guest's mount namespace
- F8: a relative `pathname` resolves against the calling process's working directory, inside its root and mount namespace
- F9: object types: regular files, directories, device nodes, FIFOs and sockets bound in the filesystem all take the new mode the same way. On a directory, `S_ISVTX` is the restricted-deletion flag and `S_ISGID` makes new entries inherit the directory's group.
- F10: host-config failure: the inode is immutable or append-only: `EPERM`. Setting either flag needs `CAP_LINUX_IMMUTABLE` in the initial user namespace, which the guest (root only in its own user namespace) lacks, so only files the host flagged give it.
- F11: host-transient failure: `EIO` from failing host storage.

## Determinism analysis

The result is a function of the path, the filesystem state along it, the inode's owner, group and flags, the caller's credentials and the mount flags. The stored mode is a pure function of `mode` and those inputs. Nothing in the call depends on timing. Under Hermit, threads run one at a time, so a `chmod` racing a `rename` or `unlink` of the same path in another guest thread resolves in a fixed order. The call repeats exactly when the guest-visible filesystem and the container credentials start in the same state and nothing outside the container changes them. That condition is why the classification is `conditional`: the legacy text was right that a stable return code in one environment is not proof by itself, and Hermit holds the filesystem stable by assumption, not by modelling it.

### Nondeterminism sources

- Changes to the filesystem made outside the container, including chmod, rename or unlink of the same object by a host process.
- Host ownership of files Hermit did not create: files in the launch image owned by a uid that the guest's user namespace does not map give `EPERM` (F5), and that ownership is part of the host filesystem image.
- Mount policy and inode flags: read-only mounts, immutable or append-only flags, and `EIO`.
- The new ctime, which the call sets but does not return; it is visible only through the stat family, which owns its virtualization.

### Cross-host variation

- Inode flags on host files the guest sees: an immutable or append-only flag set on the host gives `EPERM` (F10) on that host and not on another.

## Hermit design

Target policy: `passthrough`. The code already does this at hermit f4055eb9cae4. `chmod` is classified `PassThrough` at `detcore/src/syscall_classification.rs:727`, under the comment at `detcore/src/syscall_classification.rs:725` that these path mutations "repeat given stable guest-visible filesystem state with no external mutation", in the pass-through block that starts at `detcore/src/syscall_classification.rs:690`. The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) pins `Sysno::chmod` as `PassThrough` at `detcore/src/syscall_classification.rs:2045`, and the negative case at `detcore/src/syscall_classification.rs:2810` of the test `ownership_change_family_is_determinized_not_passthrough` keeps `chmod` and `fchmod` out of the ownership-change no-op set that determinizes `chown`.

The 2026-08-07 strategy draft proposed a deterministic filesystem metadata model keyed by resolved object identity, or else recording the result and validating identity on replay. This body does not adopt it, for these reasons:

- Hermit has no virtual filesystem. Mode bits are inode state in the guest's container mount namespace, and the guest's later `stat`, `open` and `access` calls read them from the kernel. A Detcore-side mode model would have to answer all of those too, or it would disagree with them.
- The inputs the draft worried about are either container state Hermit already holds fixed (the private `/tmp`, the launch filesystem image, the fixed container identity) or out of scope by Hermit's stated boundary (host processes changing the filesystem). The pathname races it names are between guest threads, and the scheduler already serializes them.
- `chown` needed a Determinized handler because the host identity behind the guest is not the virtual root Detcore reports (the assertion message at `detcore/src/syscall_classification.rs:2805` in that test, which starts at `detcore/src/syscall_classification.rs:2785`). `chmod` is not in that set, and its outcome does not vary between runs: the kernel's owner check (F5) runs against the fixed container identity, and the file owners it compares against come from the same fixed filesystem image. There is a fidelity boundary, not a determinism one. `getuid` and related calls are emulated as virtual root 0 (`detcore/src/syscall_classification.rs:658`), while `stat` copies `st_uid` from the kernel (`detcore/src/stat.rs:163`). Under the default user namespace the guest owns the files it creates and the check passes. Under `--no-namespace`, where the comment at `detcore/src/syscall_classification.rs:680` says the host identity behind the guest is not root, a guest that believes it is root gets `EPERM` on files it does not own, where real root would succeed. That answer repeats run to run, so it does not change `linux_determinism`; owner decision 3 accepts it as reduced fidelity (below).
- Record and replay are separate from run mode. Code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `chmod` out, and `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module not in the exported tree. The integration test `record_path_queries_and_legacy_mutations` (`hermit-cli/tests/record_replay.rs:2171`) already requires a raw `syscall(SYS_chmod, "a", 0600)` (`tests/c/record_replay_path_queries.c:237`) to replay from the recording and checks the resulting mode `0600` at `tests/c/record_replay_path_queries.c:250`, under `--backend=ptrace` (`hermit-cli/tests/record_replay.rs:562`).

The open gap is run-mode test evidence, not code. The run-mode verify cells call glibc `chmod()`: `tests/c/fchmod_bits.c:62` (mode `0644` read back by `stat`, cell `c-programs/fchmod-bits` at `tests/e2e/manifests/c-programs.yaml:11761`). On the host checked (glibc 2.34, x86_64) that wrapper issues `SYS_chmod`, but a test that pins this tracking issue should call `syscall(SYS_chmod, ...)` so that a libc switching to `fchmodat` cannot quietly remove the coverage. The legacy required tests asked for record/replay and strict repetition "on every backend that claims support"; tracking is ptrace only (owner Q3), so the tests below run on ptrace.

- Reduced fidelity (owner decision 3): under `--no-namespace` (refused by `--strict`, `hermit-cli/src/bin/hermit/run.rs:3826`) and on backends without a user namespace, the kernel checks the launching host user's uid, groups and capabilities while Detcore reports uid 0, so `chmod` on a file that user does not own fails with `EPERM` (F5) where real root would succeed, and whether `S_ISGID` is silently dropped (F2) follows the host user's groups.

## Recommended tests

- T1 (covers F1, F8): under `hermit run --verify` on ptrace, `chdir` into a fresh directory, create a regular file, and call raw `syscall(SYS_chmod, ...)` with relative paths and modes `0640`, `0600`, `07777`, `S_IFDIR | 0644` and `0`. Assert the exact `st_mode & 07777` from `stat` after each call, including that a second call replaces rather than ORs.
  Existing partial coverage via glibc `chmod()`: `tests/c/fchmod_bits.c:62` (cell `c-programs/fchmod-bits`).
- T2 (covers F2): `chown` a file the guest created to a second mapped group the guest is not a member of, drop `CAP_FSETID` from the effective set, and assert that `chmod(path, 02755)` returns 0 and stores `0755`. With `CAP_FSETID` kept, assert it stores `02755`.
- T3 (covers F3): `chmod` through a symlink to a file and assert the target's mode changes while `lstat` of the link still reports `0777`. Assert `ENOENT` through a dangling symlink and `ELOOP` through a symlink loop.
- T4 (covers F4): set an access ACL with a named-user entry and a mask through `setxattr("system.posix_acl_access")`, `chmod` to `0640`, and assert the ACL mask entry becomes `r--`. The fixture directory must be on a filesystem that supports ACLs; if it does not, the test fails rather than skips.
- T5 (covers F5): assert `EPERM` for a file owned by a uid the guest's user namespace does not map (for example a file in the launch image owned by an unmapped host uid). The immutable and append-only case is F10, recorded as U1 rather than tested.
- T6 (covers F6): assert `ENOENT` for a missing component and for `""`, `ENOTDIR` with a regular file as an intermediate component, `EACCES` with a prefix directory lacking search permission for an unmapped owner, `ENAMETOOLONG` for a 256-byte component and a path over 4096 bytes, and `EFAULT` for `pathname = (char *)1`.
- T7 (covers F7): assert `EROFS` under a read-only mount the guest can see. `EIO` is F11, recorded as U2 rather than tested.
- T8 (covers F9): `chmod` a directory to `01777` and `02775`, a FIFO made with `mknod`, and a bound `AF_UNIX` socket path, and assert each stored mode. Assert that a file created in the setgid directory takes the directory's group.
- T9 (covers F1, F3): race `chmod(path)` in one guest thread against `rename` and `unlink` of `path` in another under `hermit run --verify`, and require the same outcome (errno and final mode) on both runs.
- T10 (covers F1): record/replay: `hermit record` with `--verify` replays a raw `SYS_chmod` success and an `ENOENT` failure from the recording. Existing coverage for the success case: `tests/c/record_replay_path_queries.c:237`, driven by `hermit-cli/tests/record_replay.rs:2171`.
- U1 (covers F10; host-config): only the host can set the immutable or append-only flag, so CI produces this `EPERM` only on a host that flagged a file the guest sees. Hermit forwards the kernel's answer unchanged (`detcore/src/syscall_classification.rs:727`, `detcore/src/lib.rs:2843`).
- U2 (covers F11; host-transient): `EIO` comes from failing host storage shared with other processes. The plain passthrough (`detcore/src/lib.rs:2843`) hands it to the guest today. F11 counts toward done only once Hermit hides this error or reports it as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-08-07 · hermit 303113c10522 · orc-coord-014 · filed (legacy variant C): The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows. chmod is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant C of https://github.com/rrnewton/hermit/issues/1940: linux_determinism not stated in the legacy body; target_policy from no legacy recommended action; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: set conditional/passthrough (target_policy investigate -> passthrough: forwarded at detcore/src/lib.rs:2843, classified at detcore/src/syscall_classification.rs:727; the 2026-08-07 metadata-model draft is not adopted because Hermit has no filesystem model and the inputs it worried about are fixed container state); listed 9 flag behaviours and 10 tests from the legacy required tests; no coverage token recorded because no run-mode ptrace cell calls SYS_chmod directly, so tracking_status stays todo; open question for the owner: under --no-namespace a guest shown uid 0 gets EPERM on files it does not own (fidelity, not determinism)
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (inode flags on host files); immutable or append-only EPERM split from F5 into new F10 (host-config U1); EIO split from F7 into new F11 (host-transient U2), F7 keeps EROFS; T5 and T7 narrowed; the open question on unreachable host errors is resolved by decision 1
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d34 (owner decisions 3 and 4): added the reduced-fidelity line (EPERM on files the host user does not own, F5, and host-group S_ISGID clearing under --no-namespace and namespace-less backends); target_policy unchanged (passthrough); no new F/T ids; removed the --no-namespace fidelity open question

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `chmod`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+chmod&body=Discussion+for+the+%60chmod%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fchmod.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60chmod%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-08-07 as "Syscall classification: chmod" (https://github.com/rrnewton/hermit/issues/1940), legacy variant C.

Original filing tag: `[orc-coord-014] [coordinator, gpt-5.6-sol] [orc-coord-014]`.

### Open questions

- The new ctime is outside this issue. It belongs to the stat family tracking.

### Legacy strategy, required tests and acceptance text (2026-08-07)

Strategy draft (not adopted; see Hermit design): "Apply chmod through a deterministic filesystem metadata model keyed by resolved object identity. Serialize pathname races and model the mode-bit transformation and errors; otherwise use recorded result plus replay-side identity validation."

Legacy required tests, now T1-T10:

- Cover regular files, directories, symlinks, missing paths, denied ownership, and read-only mounts. (T1, T3, T5, T6, T7, T8)
- Check exact resulting mode bits and errno, including special-bit clearing rules. (T1, T2)
- Race rename/unlink against chmod under the scheduler and require repeatable outcomes. (T9)
- Run strict repetition and record/replay on every backend that claims support. (T9, T10; ptrace only under owner Q3)

Legacy acceptance text: "Move this row out of PASSTHROUGH only when implementation and tests bind the result to deterministic state. A future implementation is new syscall support and must follow the repository's human-review audit-tag policy." The v1 done rule in Done criteria replaces it; the target policy is now passthrough, so no move out of the pass-through table is planned.

### Legacy state and denominator (2026-08-07)

Source authority: rrnewton/hermit main at 303113c10522360de9051e21ed2d777c3436e17b.

The pinned x86-64 Sysno denominator is 373/373 classified: 289 DETERMINIZED, 83 PASSTHROUGH, and 1 UNSUPPORTED. The release tracking denominator is the 84/373 non-determinized rows.

chmod is PASSTHROUGH. It reaches the host through the blanket pass-through dispatcher; it must not be counted as determinized coverage.

### 2026-10-03 glibc wrapper check

On the measurement host (glibc 2.34, x86_64), `strace -e trace=chmod,fchmodat,fchmodat2` of a program calling glibc `chmod()` showed `chmod(".../f", 0644) = 0`: the wrapper issues `SYS_chmod`, not `fchmodat`.
