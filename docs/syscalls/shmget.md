---
schema: syscall-pages/v1
syscall: shmget
nr_x86_64: 29
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/473
  - https://github.com/rrnewton/hermit/issues/479
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmget&body=Discussion+for+the+%60shmget%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmget.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmget%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_unsupported_async_ipc_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 1
---

## Summary

`shmget(key, size, shmflg)` creates or looks up a System V shared-memory segment and returns its kernel-assigned identifier. Detcore refuses it: every call returns a fixed `ENOSYS` without reading the arguments or reaching the kernel (`detcore/src/lib.rs:2237`), as on a kernel built without System V IPC. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [shmget(2)](https://man7.org/linux/man-pages/man2/shmget.2.html).

## Linux semantics

The identifier is an integer that encodes a slot index and a per-slot sequence number in the caller's IPC namespace. It is namespace-global: any process in the same namespace that knows the integer can use it, subject to the segment's permission bits. A new segment is zero-filled, its size is rounded up to whole pages, and it lives until `shmctl(IPC_RMID)` and the last detach, not until its creator exits (unless the `kernel.shm_rmid_forced` sysctl is set). The namespace's `kernel.shmmax`, `kernel.shmall` and `kernel.shmmni` sysctls set the limits, and `kernel.shm_next_id` can preset the next identifier.

Host evidence (details under Additional information): a nonexistent key without `IPC_CREAT` failed with `ENOENT` (`shmget(0x7ffffff2, 0, 000)`), and a private 4096-byte request succeeded with a kernel-chosen identifier (`shmget(IPC_PRIVATE, 4096, IPC_CREAT|0600) = 20`).

### Flags and options

- F1: `key == IPC_PRIVATE`: always creates a new segment, whether or not `IPC_CREAT` is given. The key of the new segment is 0, so it is found only by its identifier.
- F2: `IPC_CREAT` with a non-private key: creates the segment if the key does not exist, otherwise returns the existing identifier.
- F3: `IPC_CREAT | IPC_EXCL`: fails with `EEXIST` if the key already exists.
- F4: no `IPC_CREAT`: returns the identifier of an existing key, or fails with `ENOENT` if there is none.
- F5: the low nine bits of `shmflg` are the permission mode of a new segment (execute bits are ignored). When an existing key is looked up, those bits are checked against the segment's mode and a missing permission fails with `EACCES`.
- F6: `SHM_HUGETLB`, optionally with a page-size encoding such as `SHM_HUGE_2MB` or `SHM_HUGE_1GB` in bits 26-31: back the segment with huge pages. It needs `CAP_IPC_LOCK` or membership in the `vm.hugetlb_shm_group` group (`EPERM` otherwise) and enough huge pages (`ENOMEM` otherwise).
- F7: `SHM_NORESERVE`: do not reserve swap space for the segment.
- F8: `size`: rounded up to a page multiple. Creating with `size` below `SHMMIN` (1) or above `kernel.shmmax` fails with `EINVAL`; looking up an existing key with a `size` larger than the segment fails with `EINVAL` (0 is accepted). Exceeding `kernel.shmall` or `kernel.shmmni` fails with `ENOSPC`, and a failed memory allocation with `ENOMEM`.

## Determinism analysis

`linux_determinism: conditional`, kept from the primary issue. The result is a function of the key, the flags, and the IPC namespace's state: which keys exist and with what permissions, the limits, and the allocation history that decides the next identifier. In a fresh private IPC namespace with a deterministic call order, identifiers and errors repeat. In a namespace shared with other processes, they do not: another process can create or remove the key, use up identifiers or hit the limits.

Duplicate https://github.com/rrnewton/hermit/issues/479 classified the call as NONDETERMINISTIC. This body keeps `conditional`, because the variation comes from namespace state that other processes share, not from the call itself; the `ENOENT` path and a fresh private namespace both repeat. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- The kernel-assigned identifier, which depends on earlier allocations in the namespace and on `kernel.shm_next_id`
- Keys and permissions created by other processes in the same namespace, including concurrent creators racing with `IPC_CREAT | IPC_EXCL`
- Limits (`kernel.shmmax`, `kernel.shmall`, `kernel.shmmni`), the huge page pool, memory pressure, and the caller's capabilities
- The segment is memory shared between processes, so its contents depend on the order of other attachers' writes (see `shmat`)

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `shmget` as Determinized (`detcore/src/syscall_classification.rs:446`), in the block for System V semaphores and shared memory, added under `TODO-HUMAN-REVIEW(PR-859)` (`detcore/src/syscall_classification.rs:428`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1085`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::shmget` (`detcore/src/syscall_classification.rs:2167`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F8 all return `ENOSYS` under Hermit. A request Linux would grant creates nothing, and a request Linux rejects (`EEXIST`, `ENOENT`, `EACCES`, `EPERM`, `EINVAL`, `ENOSPC`, `ENOMEM`) also gets `ENOSYS`. A guest that probes for System V shared memory sees it as absent.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `shmget`:

- A created segment is backed by Detcore-owned memory the guest cannot name, for example a memfd, sized to the page-rounded request and zero-filled; `shmat` maps it. F1 to F5 then behave as on Linux against the Detcore namespace: a key exists only if a guest created it in this run.
- F6: `SHM_HUGETLB` answers as a host with an empty 2 MiB huge-page pool and no 1 GiB page size would. A page-size encoding other than 0 (the default) or 21 (2 MiB) gives `EINVAL`; a caller without `CAP_IPC_LOCK`, outside the record's hugetlb group, and over `RLIMIT_MEMLOCK` gets `EPERM`; anything else gets `ENOMEM`. PostgreSQL's `huge_pages=try` falls back to ordinary memory on that error.
- F7: `SHM_NORESERVE` is accepted and has no effect.
- F8: the limits are the defaults above, so SHMMNI (4096) is the only one a guest can reach in practice.

Errno order (owner decision 6), following `ksys_shmget`, `ipcget_public` and `newseg` in Linux `ipc/shm.c` and `ipc/util.c`. With a non-private key, the key is looked up first. If it is absent, a call without `IPC_CREAT` fails with `ENOENT`; otherwise the segment is created. If it is present, `IPC_CREAT | IPC_EXCL` fails with `EEXIST`, then a `size` larger than the segment fails with `EINVAL`, then a missing permission fails with `EACCES`. Creation, which `IPC_PRIVATE` always takes, checks in this order: `size` below 1 or above SHMMAX (`EINVAL`); a page count that overflows or would exceed SHMALL (`ENOSPC`); allocation (`ENOMEM`); for `SHM_HUGETLB`, an unknown page size (`EINVAL`), then the capability rule (`EPERM`), then the pool (`ENOMEM`); and last the SHMMNI count (`ENOSPC`).

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F1, F5, F8): in a fresh run, `shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600)` returns 0 and a second identical call returns 1. `shmctl(IPC_STAT)` on the first shows key 0, `shm_segsz` 4096, mode 0600, `shm_cpid` the caller's virtual pid and `shm_nattch` 0. The raw `syscall(SYS_shmget, 0, 0, 0)` (`IPC_PRIVATE` with `size` 0) returns `EINVAL`. Compare with the native run in a fresh IPC namespace, except pids and times.
  Existing candidates: `tests/c/sysv_shm_enosys.c` issues the raw call (cell `c-programs/sysv-shm-enosys`, `tests/e2e/manifests/c-programs.yaml:9730`, verify ci on ptrace at line 9749); the cargo test `sysv_and_legacy_filesystem_features_fall_back_deterministically` (`hermit-cli/tests/sysv_legacy_fallbacks.rs:30`) runs that program with `--backend=ptrace run --strict --verify --panic-on-unsupported-syscalls` (`hermit-cli/tests/sysv_legacy_fallbacks.rs:59`); `tests/c/sysv_ipc_refusal.c:50` issues the granted request (cell `c-programs/sysv-ipc-refusal`, `tests/e2e/manifests/c-programs.yaml:14033`, verify ci true with ptrace enabled at line 14053). These programs assert today's `ENOSYS`; they must change to the Linux results above when the model lands.
- T2 (covers F2, F3, F4, F5): with a fixed key K, flags 0 gives `ENOENT`; `IPC_CREAT | 0600` creates the segment; a second `IPC_CREAT | 0600` returns the same identifier; `IPC_CREAT | IPC_EXCL | 0600` gives `EEXIST`; flags 0 now returns the identifier. After the guest drops to uid 1000 with no capabilities (owner decision 4), a lookup of K gives `EACCES`. The harness creates a host segment with key K before the run: the guest's first lookup still gives `ENOENT`, and afterwards the host's `/proc/sysvipc/shm` holds only the harness's segment. Compare with the native run in a fresh IPC namespace, except pids and times.
- T3 (covers F6, F7): `SHM_NORESERVE` succeeds; `SHM_HUGETLB` and `SHM_HUGETLB | SHM_HUGE_2MB` give `ENOMEM`; `SHM_HUGETLB | SHM_HUGE_1GB` gives `EINVAL`. The model fixes these results whatever the host's huge-page pool holds, so this test does not compare them with a native run.
- T4 (covers F8): `size` `ULONG_MAX` with `IPC_PRIVATE` gives `EINVAL`; a lookup of an existing key with a `size` larger than the segment gives `EINVAL` and with `size` 0 succeeds; 4096 private segments are created and the 4097th gives `ENOSPC`.
- T5 (covers F1): record a guest that creates a segment, attaches it, writes and reads it back, then replay it; both phases see the same identifiers and bytes. Both e2e cells above disable record/replay on ptrace (for example `tests/e2e/manifests/c-programs.yaml:9770`), so this needs a replay cell or a cargo test.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/473: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/479 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every shmget call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays conditional; 8 flag behaviours and 5 tests listed; merged duplicate https://github.com/rrnewton/hermit/issues/479; kept every fact, strace line and test idea of the duplicate; its NONDETERMINISTIC classification lost to conditional (see Determinism analysis); owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6, 4 and 13 applied (decision 14: shmget never waits); target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the shmget target and the kernel errno order (decision 6); T1-T5 rewritten as behaviour tests, ids and covers kept except T2 now also covers F5; no new F/T ids; removed both open questions (keep refuse or model, answered by decision 9; private IPC namespace, answered: the guest shares the host IPC namespace, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `shmget`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmget&body=Discussion+for+the+%60shmget%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmget.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmget%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: shmget" (https://github.com/rrnewton/hermit/issues/473), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=shmget` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `shmget` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
shmget(0x7ffffff2, 0, 000)      = -1 ENOENT (No such file or directory)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate

Duplicate https://github.com/rrnewton/hermit/issues/479 was filed 2026-07-24 as "Syscall classification: shmget" (legacy variant A1) and is retired into this issue. Its comment history (0 comments) stays on that issue; this issue's own history is on https://github.com/rrnewton/hermit/issues/473. Its facts were merged above: the man page link, the `IPC_PRIVATE` behaviour, the host-state sources, and the private-segment probe. Its NONDETERMINISTIC classification lost to `conditional`, for the reason under Determinism analysis, and its legacy recommended action NEEDS-HANDLER lost to `refuse`, for the reason under Hermit design. Its host evidence, kept verbatim:

> Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=shmget`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `shmget` in the UNCLASSIFIED temporary-passthrough block.

```text
shmget(IPC_PRIVATE, 4096, IPC_CREAT|0600) = 20
```

The probe created a private 4096-byte segment and then removed it. The "temporary-passthrough" statements in both evidence blocks describe the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design).

### Notes

- The generated inventory `tests/e2e/manifests/inventory/test-files.json:2028` describes `tests/c/sysv_shm_enosys.c` as `ci=false`, while its manifest cell enables verify CI on ptrace (`tests/e2e/manifests/c-programs.yaml:9749`). The manifest is what the harness runs; the inventory text looks stale.
