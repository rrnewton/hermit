---
schema: syscall-pages/v1
syscall: shmctl
nr_x86_64: 31
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/471
  - https://github.com/rrnewton/hermit/issues/476
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmctl&body=Discussion+for+the+%60shmctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmctl%60%2C+or+in+what+the+page+says%3F%0A
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

`shmctl(shmid, cmd, buf)` queries or changes a System V shared-memory segment, or reports system-wide limits and usage. Detcore refuses it: every command returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2237`). Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [shmctl(2)](https://man7.org/linux/man-pages/man2/shmctl.2.html).

## Linux semantics

The status structure `shmid_ds` carries the owner and creator ids, the mode, the size, the attach count `shm_nattch`, the creator and last-operation pids (`shm_cpid`, `shm_lpid`), and the wall-clock times of the last attach, detach and change (`shm_atime`, `shm_dtime`, `shm_ctime`). The info commands return their result as the index of the highest slot in use, not 0.

Host evidence (details under Additional information): an invalid identifier failed with `EINVAL` (`shmctl(-1, IPC_STAT, NULL)`), and `IPC_STAT` on a live private segment returned host uid and gid, the creator's host pid and a wall-clock `shm_ctime`.

### Flags and options

- F1: `IPC_STAT`: copy the segment's `shmid_ds` to `buf`. Needs read permission (`EACCES` otherwise).
- F2: `IPC_SET`: set the owner uid and gid and the low nine mode bits from `buf`, and update `shm_ctime`. Only the owner, the creator or a `CAP_SYS_ADMIN` caller may do it (`EPERM` otherwise).
- F3: `IPC_RMID`: mark the segment for removal. Its key becomes private at once, so new `shmget` lookups no longer find it, and it is destroyed after the last detach. Same permission rule as `IPC_SET`.
- F4: `IPC_INFO` (Linux-specific): copy the system limits (`struct shminfo`: `shmmax`, `shmmin`, `shmmni`, `shmseg`, `shmall`) to `buf`; `shmid` is ignored.
- F5: `SHM_INFO` (Linux-specific): copy system-wide usage (`struct shm_info`: segments in use, total, resident and swapped pages) to `buf`.
- F6: `SHM_STAT` and `SHM_STAT_ANY` (Linux-specific): like `IPC_STAT`, but `shmid` is an index into the kernel's slot table, and the call returns the identifier of that slot. `SHM_STAT_ANY` (Linux 4.17 and later) skips the read-permission check.
- F7: `SHM_LOCK` and `SHM_UNLOCK` (Linux-specific): keep the segment's pages from being swapped out, or allow it again. An unprivileged caller is limited by `RLIMIT_MEMLOCK`; `CAP_IPC_LOCK` lifts the limit.
- F8: errors: an unknown `cmd` or an identifier (or index) that names no segment gives `EINVAL`, and an unreadable `buf` gives `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`, kept from the primary issue. The commands that change state (F2, F3, F7) and the errors (F8) depend only on the arguments and the segment. The query commands do not repeat across runs: `IPC_STAT`, `SHM_STAT` and `SHM_STAT_ANY` return host pids, host ids, wall-clock times and an attach count that other processes change, and `IPC_INFO`/`SHM_INFO` return host-wide limits and usage. `IPC_RMID` and `IPC_SET` also change an object other processes can see.

Duplicate https://github.com/rrnewton/hermit/issues/476 classified the call as NONDETERMINISTIC. This body keeps `conditional`, because the nondeterminism depends on the command: the status and info commands expose host state, while the change commands and the error paths repeat. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- `shm_cpid` and `shm_lpid`, which are host pids, and `shm_perm` uid and gid, which are host ids
- `shm_atime`, `shm_dtime` and `shm_ctime`, which are wall-clock times
- `shm_nattch`, changed by other processes attaching and detaching
- System-wide limits and usage returned by `IPC_INFO` and `SHM_INFO`, and the slot indexes used by `SHM_STAT`
- Concurrent processes changing or removing the same segment

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `shmctl` as Determinized (`detcore/src/syscall_classification.rs:442`), in the block for System V semaphores and shared memory, added under `TODO-HUMAN-REVIEW(PR-859)` (`detcore/src/syscall_classification.rs:428`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1081`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::shmctl` (`detcore/src/syscall_classification.rs:2165`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F8 all return `ENOSYS` under Hermit. `buf` is neither read nor written, no segment is changed, locked or removed, and the error paths that would give `EINVAL` or `EFAULT` also give `ENOSYS`.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `shmctl`:

- `IPC_STAT`, `SHM_STAT` and `SHM_STAT_ANY` (F1, F6) return the Detcore segment's fields: virtual pids and times, uid and gid from the decision-4 record, and the attach count from the attachment lists. `SHM_STAT` takes a slot index of the Detcore namespace.
- `IPC_SET` and `IPC_RMID` (F2, F3) change only the Detcore segment. After `IPC_RMID` the key becomes private, the `SHM_DEST` mode bit is set, and the backing memory is freed at the last detach.
- `IPC_INFO` (F4) returns the defaults above, with `shmseg` equal to SHMMNI (4096) as on Linux. `SHM_INFO` (F5) reports only guest segments: `used_ids`, `shm_tot` in pages, `shm_rss` equal to `shm_tot` and `shm_swp` 0, because host residency and swap are not deterministic. Both return the highest slot index in use.
- `SHM_LOCK` and `SHM_UNLOCK` (F7) set or clear the `SHM_LOCKED` mode bit after the permission checks; they lock no host memory.

Errno order (owner decision 6), following `ksys_shmctl` in Linux `ipc/shm.c`: a negative `cmd` or `shmid` gives `EINVAL` before anything else. `IPC_INFO` and `SHM_INFO` can then fail only with `EFAULT` on the copy-out. The stat commands look the segment up (`EINVAL`), check read permission (`EACCES`, skipped for `SHM_STAT_ANY`), and copy out last (`EFAULT`). `IPC_SET` copies `buf` in first (`EFAULT`); `IPC_SET` and `IPC_RMID` then look the segment up (`EINVAL`) and require the owner, the creator or `CAP_SYS_ADMIN` (`EPERM`). `SHM_LOCK` and `SHM_UNLOCK` look the segment up (`EINVAL`), then give `EPERM` to a caller without `CAP_IPC_LOCK` that is neither owner nor creator, or that asks for `SHM_LOCK` with a zero `RLIMIT_MEMLOCK`. Any other `cmd` gives `EINVAL`.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F1, F4, F5, F6): on a guest segment, `IPC_STAT` gives the expected size, mode, uid 0 and virtual pids; `IPC_INFO` gives `shmmax` and `shmall` 18446744073692774399, `shmmin` 1, `shmmni` 4096 and `shmseg` 4096; `SHM_INFO` gives `used_ids` 1; `SHM_STAT` and `SHM_STAT_ANY` on index 0 return the segment's identifier. Compare with the native run in a fresh IPC namespace, except pids and times. `shm_rss` and `shm_swp` are not compared.
- T2 (covers F2, F3, F7): `IPC_SET` changes the mode and `shm_ctime`; `SHM_LOCK` and `SHM_UNLOCK` set and clear `SHM_LOCKED` in the mode; `IPC_RMID` sets `SHM_DEST`, a lookup of the key then gives `ENOENT`, and an existing attachment keeps working until it is detached. After the guest drops to uid 1000 with no capabilities (owner decision 4), `IPC_SET` and `IPC_RMID` on a segment owned by uid 0 give `EPERM`. A host segment whose identifier the harness passes in is left unchanged.
- T3 (covers F8): `cmd` 99 gives `EINVAL`; `cmd` -1 and `shmid` -1 give `EINVAL` even with a NULL `buf`; `IPC_STAT` with `buf == NULL` gives `EFAULT` on a guest segment and `EINVAL` on an unknown identifier, because the lookup comes before the copy-out.
- T4 (covers F1): record a guest that calls `shmctl(IPC_STAT)` on its own segment, then replay it; both phases see the same fields.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/471: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/476 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every shmctl call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays conditional; 8 flag behaviours and 4 tests listed; merged duplicate https://github.com/rrnewton/hermit/issues/476; kept every fact, strace line and test idea of the duplicate; its NONDETERMINISTIC classification lost to conditional (see Determinism analysis); owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 4 and 13 applied (decision 14: shmctl never waits); target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the shmctl target (SHM_INFO reports shm_rss = shm_tot and shm_swp 0, SHM_LOCK only sets the mode bit) and the kernel errno order (decision 6); T1-T4 rewritten as behaviour tests, ids and covers kept; no new F/T ids; removed both open questions (answered by decision 9 and by the host IPC namespace finding, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `shmctl`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmctl&body=Discussion+for+the+%60shmctl%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmctl.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmctl%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: shmctl" (https://github.com/rrnewton/hermit/issues/471), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=shmctl` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `shmctl` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
shmctl(-1, IPC_STAT, NULL)      = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate

Duplicate https://github.com/rrnewton/hermit/issues/476 was filed 2026-07-24 as "Syscall classification: shmctl" (legacy variant A1) and is retired into this issue. Its comment history (0 comments) stays on that issue; this issue's own history is on https://github.com/rrnewton/hermit/issues/471. Its facts were merged above: the man page link, the fields `IPC_STAT` exposes, the command list including locking and the Linux-specific queries, and the live-metadata probe. Its NONDETERMINISTIC classification lost to `conditional`, for the reason under Determinism analysis, and its legacy recommended action NEEDS-HANDLER lost to `refuse`, for the reason under Hermit design. Its host evidence, kept verbatim:

> Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=shmctl`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `shmctl` in the UNCLASSIFIED temporary-passthrough block.

```text
shmctl(18, IPC_STAT, {shm_perm={uid=212630, gid=100, mode=0600, key=0, cuid=212630, cgid=100}, shm_segsz=4096, shm_cpid=3143004, shm_lpid=0, shm_nattch=0, shm_atime=0, shm_dtime=0, shm_ctime=1784864190}) = 0
```

The "temporary-passthrough" statements in both evidence blocks describe the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design). No test file at that SHA names `shmctl`.
