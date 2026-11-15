---
schema: syscall-pages/v1
syscall: shmat
nr_x86_64: 30
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/470
  - https://github.com/rrnewton/hermit/issues/475
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmat&body=Discussion+for+the+%60shmat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmat%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unsupported_async_ipc_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`shmat(shmid, shmaddr, shmflg)` maps a System V shared-memory segment into the caller's address space and returns the address. Detcore refuses it: every call returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2237`), so no host segment is ever mapped into a guest. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [shmop(2)](https://man7.org/linux/man-pages/man2/shmop.2.html).

## Linux semantics

A successful attach maps the whole segment (rounded to pages) as a shared mapping, increments the segment's attach count `shm_nattch`, and sets `shm_atime` (wall-clock time) and `shm_lpid` (the caller's pid). Attachments are inherited across `fork` and removed at `execve` and exit. Linux, unlike POSIX, also allows attaching a segment already marked for removal with `IPC_RMID`, as long as it still exists.

Host evidence (details under Additional information): an invalid identifier failed with `EINVAL` (`shmat(-1, NULL, 0)`), and a private segment attached at a kernel-chosen address (`shmat(17, NULL, 0) = 0x7fc710d68000`).

### Flags and options

- F1: `shmaddr == NULL`: the kernel chooses the address, as for `mmap` without a hint, so the result depends on the address-space layout (ASLR).
- F2: `shmaddr` non-NULL without `SHM_RND`: the address must be page aligned (`SHMLBA` is the page size on x86_64), otherwise `EINVAL`. If the range overlaps an existing mapping and `SHM_REMAP` is not given, the call fails with `EINVAL`.
- F3: `SHM_RND`: round a non-NULL `shmaddr` down to a multiple of `SHMLBA`.
- F4: `SHM_REMAP` (Linux-specific): replace any mapping in the range, like `MAP_FIXED`. With `shmaddr == NULL` it fails with `EINVAL`.
- F5: `SHM_RDONLY`: attach read-only, which needs read permission on the segment. Without it the attach is read-write and needs read and write permission. A missing permission fails with `EACCES`.
- F6: `SHM_EXEC` (Linux-specific): map the segment executable, which also needs execute permission on the segment.
- F7: `shmid` that names no segment fails with `EINVAL`. A failed mapping (address space or `vm.max_map_count` exhausted) fails with `ENOMEM`.
- F8: side effects of success: `shm_nattch` increments, `shm_atime` and `shm_lpid` change, the mapping is inherited by `fork` children, and a segment marked with `IPC_RMID` can still be attached while it exists.

## Determinism analysis

`linux_determinism: conditional`, kept from the primary issue. Given a segment, flags and a fixed address, the attach is decided by the caller's own address space. With `shmaddr == NULL` the address follows the layout, which repeats only with a fixed layout. The segment's contents are written by every attached process, so what the caller reads depends on the order of other processes' writes. The status fields the attach updates (`shm_atime`, `shm_lpid`) carry wall-clock time and host pids.

Duplicate https://github.com/rrnewton/hermit/issues/475 classified the call as NONDETERMINISTIC. This body keeps `conditional`: the address and errors repeat for a fixed layout and a fixed segment, and the variation comes from the layout and from other writers. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- The attach address chosen by the kernel (ASLR and the current layout) when `shmaddr` is NULL
- Segment contents written by other attached processes, inside or outside the guest, and the order of those writes
- Host IPC namespace contents: which identifiers exist and with what permissions
- `shm_atime` (wall-clock) and `shm_lpid` (host pid) written by the attach, later visible through `shmctl(IPC_STAT)`

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `shmat` as Determinized (`detcore/src/syscall_classification.rs:440`), in the block for System V semaphores and shared memory, added under `TODO-HUMAN-REVIEW(PR-859)` (`detcore/src/syscall_classification.rs:428`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1079`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::shmat` (`detcore/src/syscall_classification.rs:2164`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F8 all return `ENOSYS` under Hermit. No mapping is created or replaced, so `SHM_REMAP` (F4) cannot clobber a guest mapping, and the host segment's attach count, times and pid are untouched. Because `shmget` is refused too, a guest has no identifier of its own; a guessed host identifier is refused the same way.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `shmat`:

- Detcore injects a `MAP_SHARED` mapping of the segment's backing memory: with `MAP_FIXED` at `shmaddr` (rounded down under `SHM_RND`, F3) when one is given, and without a hint when `shmaddr` is NULL (F1), so the address is whatever a hintless guest `mmap` gets under Hermit. `SHM_REMAP` (F4) replaces mappings in the range; without it an overlap gives `EINVAL` (F2). Protection follows `SHM_RDONLY` and `SHM_EXEC` (F5, F6), checked against the segment's mode and the decision-4 record.
- Detcore keeps a per-process attachment list: `fork` children inherit it, `execve` and exit detach everything, and a guest `munmap` or `mremap` over an attachment counts as a detach. Success increments `shm_nattch` and sets `shm_lpid` to the caller's virtual pid and `shm_atime` to virtual time (F8). A segment marked `IPC_RMID` stays attachable until its last detach, as on Linux.

Errno order (owner decision 6), following `do_shmat` in Linux `ipc/shm.c`: `shmid` below 0 gives `EINVAL`; then the address rules give `EINVAL` (an unaligned `shmaddr` without `SHM_RND`, `SHM_RND` rounding to 0 with `SHM_REMAP`, or `SHM_REMAP` with a NULL `shmaddr`); then the lookup gives `EINVAL`; then the permission check gives `EACCES` (read for `SHM_RDONLY`, read and write otherwise, plus execute for `SHM_EXEC`); then a segment removed in the meantime gives `EIDRM`; then an overlap without `SHM_REMAP` gives `EINVAL`; and last the mapping itself can give `ENOMEM`.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F1, F5, F7): `shmat(-1, NULL, 0)` gives `EINVAL`, and so does an identifier the guest never created. On a guest segment, `shmat(id, NULL, 0)` returns an address that is the same in both `--verify` runs, and `SHM_RDONLY` gives a read-only mapping (a write is caught by a `SIGSEGV` handler and the guest exits normally). After the guest drops to uid 1000 with no capabilities (owner decision 4), attaching a 0600 segment owned by uid 0 gives `EACCES`. Compare with the native run in a fresh IPC namespace, except pids and times.
- T2 (covers F2, F3, F4, F6): a page-aligned `shmaddr` attaches there; an unaligned one gives `EINVAL` without `SHM_RND` and attaches at the rounded-down address with it; an aligned address overlapping a guest mapping gives `EINVAL` without `SHM_REMAP` and replaces that mapping with it; `SHM_REMAP` with a NULL `shmaddr` gives `EINVAL`; `SHM_EXEC` gives an executable mapping.
- T3 (covers F7, F8): the parent creates and attaches a segment and forks; the child, which inherits the attachment, writes a pattern and exits; the parent reads the pattern, and `IPC_STAT` shows `shm_nattch` 1, then 2, then 1 again, with `shm_lpid` set to a virtual pid. After `IPC_RMID` the attached segment can still be attached again. The harness creates a host segment and passes its identifier: the guest, having created no segment, gets `EINVAL`, and the host segment's `shm_nattch` stays 0.
- T4 (covers F1): record a guest that attaches a segment and exchanges data through it between two processes, then replay it; both phases see the same addresses and bytes.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/470: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/475 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every shmat call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays conditional; 8 flag behaviours and 4 tests listed; merged duplicate https://github.com/rrnewton/hermit/issues/475; kept every fact, strace line and test idea of the duplicate; its NONDETERMINISTIC classification lost to conditional (see Determinism analysis); owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 4 and 13 applied (decision 14: shmat never waits); target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the shmat target (injected MAP_SHARED mapping, per-process attachment list) and the kernel errno order (decision 6); T1-T4 rewritten as behaviour tests, ids and covers kept; no new F/T ids; removed both open questions (answered by decision 9 and by the host IPC namespace finding, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `shmat`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmat&body=Discussion+for+the+%60shmat%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmat.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmat%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: shmat" (https://github.com/rrnewton/hermit/issues/470), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=shmat` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `shmat` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
shmat(-1, NULL, 0)              = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate

Duplicate https://github.com/rrnewton/hermit/issues/475 was filed 2026-07-24 as "Syscall classification: shmat" (legacy variant A1) and is retired into this issue. Its comment history (0 comments) stays on that issue; this issue's own history is on https://github.com/rrnewton/hermit/issues/470. Its facts were merged above: the man page link, the observable address, shared bytes and attach count, the ASLR and concurrent-writer sources, and the private-segment probe. Its NONDETERMINISTIC classification lost to `conditional`, for the reason under Determinism analysis, and its legacy recommended action NEEDS-HANDLER lost to `refuse`, for the reason under Hermit design. Its host evidence, kept verbatim:

> Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=shmat`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `shmat` in the UNCLASSIFIED temporary-passthrough block.

```text
shmat(17, NULL, 0)              = 0x7fc710d68000
```

The "temporary-passthrough" statements in both evidence blocks describe the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design). No test file at that SHA names `shmat`.
