---
schema: syscall-pages/v1
syscall: shmdt
nr_x86_64: 67
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/472
  - https://github.com/rrnewton/hermit/issues/477
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmdt&body=Discussion+for+the+%60shmdt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmdt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmdt%60%2C+or+in+what+the+page+says%3F%0A
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

`shmdt(shmaddr)` detaches the System V shared-memory segment attached at `shmaddr` from the caller's address space. Detcore refuses it: every call returns a fixed `ENOSYS` without reading the argument or reaching the kernel (`detcore/src/lib.rs:2237`), so no guest mapping is ever removed by it. Owner decision 9 (2026-10-04) makes the target `model`: Detcore keeps its own System V IPC namespace for the run.

Manual reference: [shmop(2)](https://man7.org/linux/man-pages/man2/shmop.2.html).

## Linux semantics

`shmaddr` must be the address a successful `shmat` returned. The kernel removes the mappings of that attachment, decrements the segment's attach count `shm_nattch`, and sets `shm_dtime` (wall-clock time) and `shm_lpid` (the caller's pid). A segment marked with `shmctl(IPC_RMID)` is destroyed when its last attachment goes away, by `shmdt`, `execve` or exit.

Host evidence (details under Additional information): an invalid address failed with `EINVAL` (`shmdt(0xffffffffffffffff)`), and a private attachment detached successfully (`shmdt(0x7fe8b68f8000) = 0`).

### Flags and options

- F1: `shmaddr` is the start of an attachment: the attachment is removed, `shm_nattch` decrements, and `shm_dtime` and `shm_lpid` change.
- F2: the detach removes the last attachment of a segment marked with `IPC_RMID`: the segment is destroyed and its memory freed.
- F3: `shmaddr` is not page aligned, or no attachment starts there (including an ordinary `mmap` mapping): `EINVAL`.

## Determinism analysis

`linux_determinism: conditional`, kept from the primary issue, and the duplicate agrees. For the caller, the result depends only on its own attachments, so the return value repeats. The detach also changes shared state: it updates the segment's attach count, detach time and last pid, and the last detach of a removed segment destroys it, which other attached processes and later `shmctl(IPC_STAT)` calls observe. Those effects depend on what other processes have attached and removed. Under Hermit none of this reaches the guest, because the call is refused.

### Nondeterminism sources

- `shm_dtime` (wall-clock) and `shm_lpid` (host pid) written by the detach
- `shm_nattch` and the `IPC_RMID` state, which other processes change, and which decide whether this detach destroys the segment
- The caller's own mappings, which decide between success and `EINVAL`

## Hermit design

Today (hermit f4055eb9cae4) Detcore refuses the call with a fixed `ENOSYS`. That is not the target; the target is under `Target (owner decision 9)` below. Today's refusal:

- `classify_syscall` lists `shmdt` as Determinized (`detcore/src/syscall_classification.rs:444`), in the block for System V semaphores and shared memory, added under `TODO-HUMAN-REVIEW(PR-859)` (`detcore/src/syscall_classification.rs:428`).
- The predicate `is_unsupported_async_ipc_syscall` (`detcore/src/syscall_classification.rs:1051`) matches it (`detcore/src/syscall_classification.rs:1083`), together with every other System V IPC call, the POSIX message queue calls and Linux native AIO. Its doc comment (`detcore/src/syscall_classification.rs:1034`) gives the reason: the System V families operate on global, key-addressed kernel objects that persist across runs and are shared with the whole host, so forwarding them is nondeterministic and a container-isolation hole. `ENOSYS` is exactly what a kernel built without `CONFIG_SYSVIPC` returns.
- The dispatch arm in `handle_syscall_event` (`detcore/src/lib.rs:2237`-`2241`) returns `Err(Error::Errno(Errno::ENOSYS))` for every call. It is an arm of the `match classify_syscall(..)` at `detcore/src/lib.rs:2028`, ahead of the typed match, because the pinned Reverie has no typed form for these calls (comment at `detcore/src/lib.rs:2232`). It reads no argument and forwards nothing to the host. It does not depend on `panic_on_unsupported_syscalls`, which matches `code_facts.refused_only_when_fail_closed: false`.
- The predicate is part of `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1325`), the boundary that backends running guest syscalls outside this dispatcher consult to enforce the same refusal (`detcore/src/lib.rs:226`).
- The unit test `representative_policies_stay_in_their_reviewed_sections` (`detcore/src/syscall_classification.rs:1899`) asserts the Determinized classification and the predicate membership for `Sysno::shmdt` (`detcore/src/syscall_classification.rs:2166`, assertion at line 2170). It pins membership only; what a guest sees is exercised by the programs under Recommended tests.

Today's consequence for every flag: F1 to F3 all return `ENOSYS` under Hermit. The guest's mappings are unchanged, including an ordinary mapping at `shmaddr`, and no host segment's attach count or lifetime changes. Because `shmat` is refused too, a guest never has a System V attachment to detach.

Target (owner decision 9): `model`. On 2026-10-04 the owner accepted modelling the whole System V IPC family (`shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl`, `msgget`, `msgsnd`, `msgrcv`, `msgctl`) inside Detcore. The named workload is PostgreSQL, which needs a System V shared-memory segment, and the standing least-common-denominator policy forbids refusing an interface such a guest has no fallback for. Passthrough is ruled out because the guest shares the host's IPC namespace: at the pin, `default_container` unshares only `Namespace::PID` (`hermit-cli/src/bin/hermit/container.rs:442`-`453`, the call at line 445), `image_container` does the same (`hermit-cli/src/bin/hermit/container.rs:482`), and nothing under `hermit-cli/src` asks for `CLONE_NEWIPC`. The family shares one model:

- One Detcore-owned IPC namespace per run, empty at start and seen by every guest process. Objects exist only in Detcore and only for the run. Nothing is created in the host's namespace, so the host's `/proc/sysvipc/*` never changes, and a host identifier a guest guesses names nothing.
- Identifiers follow the kernel's rule for a fresh namespace with `kernel.*_next_id` unset (`ipc_idr_alloc`, Linux `ipc/util.c`): the slot index is allocated cyclically starting at 0, the sequence number advances when an allocation reuses an index not above the last one, and the identifier is sequence * 32768 + index. The first object of each kind gets identifier 0, and every later identifier depends only on the guest's call order.
- Limits are the compiled-in defaults of a fresh Linux IPC namespace, never the host's sysctls (`include/uapi/linux/shm.h`, `sem.h`, `msg.h`): SHMMIN 1, SHMMAX 18446744073692774399 bytes, SHMALL 18446744073692774399 pages, SHMMNI 4096; SEMMSL 32000, SEMMNS 1024000000, SEMOPM 500, SEMMNI 32000, SEMVMX 32767; MSGMAX 8192, MSGMNB 16384, MSGMNI 32000.
- Owner and creator uid and gid in `ipc_perm`, and the permission rules (`EACCES` from the mode bits unless the caller has `CAP_IPC_OWNER`; `EPERM` unless the caller is the owner or creator or has `CAP_SYS_ADMIN`), use the per-process credential record of owner decision 4. The default guest is uid 0 with every capability, so those errors appear only after a guest changes its recorded credentials.
- Pids in status structures are virtual pids, and times are virtual time (owner decision 13).

Per-call target for `shmdt`: Detcore looks `shmaddr` up in the caller's attachment list (see `shmat`). A match is unmapped by an injected `munmap`, decrements `shm_nattch`, and sets `shm_dtime` to virtual time and `shm_lpid` to the caller's virtual pid (F1). The last detach of a segment marked `IPC_RMID` frees its backing memory (F2). Anything else gives `EINVAL` and leaves the guest's mappings unchanged (F3).

Errno order (owner decision 6), following `ksys_shmdt` in Linux `ipc/shm.c`: a `shmaddr` that is not page aligned gives `EINVAL`; then an address where no attachment starts gives `EINVAL`.

Why not the other policies:

- `refuse`, today's code, is deterministic but breaks PostgreSQL, the workload decision 9 names, and the standing policy forbids refusing an interface a common guest needs and has no fallback for.
- `passthrough` was the legacy behaviour (the 2026-07-24 evidence under Additional information was gathered while the call was still forwarded). It hands the guest identifiers allocated in the host's IPC namespace, which the guest shares (see above), so results depend on every other process using System V IPC. Identifiers are small guessable integers, so a forwarded call could also reach objects the guest did not create.
- `emulate` with a fake success would hand out objects that do not behave as shared state between cooperating processes.

The arm and the classification entries carry `TODO-HUMAN-REVIEW(#731)` and `TODO-HUMAN-REVIEW(PR-859)` markers (`detcore/src/lib.rs:2221`, `detcore/src/lib.rs:2235`, `detcore/src/syscall_classification.rs:1033`, `detcore/src/syscall_classification.rs:1049`). They ask for a human review of the refusal boundary; they do not report a known defect. Owner decision 9 (2026-10-04) settles that review: the family is to be modelled, so this refusal is temporary.

Every recommended test runs under `hermit run --verify` on ptrace and asserts the Linux result its text gives. Where the test says so, it also runs the program natively in a fresh IPC namespace (`unshare --user --map-root-user --ipc`), which starts empty with the default limits, as the model does, and compares the results other than pids and times. At hermit f4055eb9cae4 each of these tests fails with `ENOSYS`; an existing program named below that asserts `ENOSYS` checks today's refusal and must change when the model lands.

## Recommended tests

- T1 (covers F3): `shmdt(NULL)`, an unaligned address and the start of an ordinary anonymous `mmap` mapping each give `EINVAL`, and that mapping still holds its bytes afterwards. Compare with the native run in a fresh IPC namespace, except pids and times.
- T2 (covers F1, F2): attach one segment twice and `shmdt` one attachment: `shm_nattch` drops from 2 to 1, `shm_dtime` and `shm_lpid` change, the other attachment still holds the bytes, and a read of the detached range is caught by a `SIGSEGV` handler. Mark the segment `IPC_RMID` and detach the last attachment: `shmctl(IPC_STAT)` on its identifier then gives `EINVAL`, and `shmctl(SHM_INFO)` reports 0 segments in use.
- T3 (covers F1): record a guest that attaches and detaches a segment, then replay it; both phases see the same results.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/472: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_unsupported_async_ipc_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/477 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed ENOSYS for every shmdt call without reading arguments or reaching the host, detcore/src/lib.rs:2237-2241); linux_determinism stays conditional; 3 flag behaviours and 3 tests listed; merged duplicate https://github.com/rrnewton/hermit/issues/477; kept every fact, strace line and test idea of the duplicate; both issues classified it conditional; owner question recorded under Open questions (keep refuse, or model System V IPC); no coverage token, so ptrace_verification stays untested and tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 9, 6 and 13 applied (decision 14: shmdt never waits); target_policy refuse->model; Target (owner decision 9) block in Hermit design with the shared model, the shmdt target and the kernel errno order (decision 6); T1-T3 rewritten as behaviour tests, ids and covers kept; no new F/T ids; removed both open questions (answered by decision 9 and by the host IPC namespace finding, hermit-cli/src/bin/hermit/container.rs:442-453)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `shmdt`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+shmdt&body=Discussion+for+the+%60shmdt%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fshmdt.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60shmdt%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: shmdt" (https://github.com/rrnewton/hermit/issues/472), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=shmdt` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit currently routes `shmdt` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
shmdt(0xffffffffffffffff)       = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*

### Merged duplicate

Duplicate https://github.com/rrnewton/hermit/issues/477 was filed 2026-07-24 as "Syscall classification: shmdt" (legacy variant A1) and is retired into this issue. Its comment history (0 comments) stays on that issue; this issue's own history is on https://github.com/rrnewton/hermit/issues/472. Its facts were merged above: the man page link, the shared attach metadata and detach timestamp, the `IPC_RMID` plus last-detach rule, and the successful-detach probe. Both issues classified the call CONDITIONAL, and the duplicate's legacy recommended action NEEDS-HANDLER lost to `refuse` (Hermit design); the primary issue's sentence "None from the call itself" is refined under Determinism analysis, because the detach writes shared metadata. Its host evidence, kept verbatim:

> Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=shmdt`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `shmdt` in the UNCLASSIFIED temporary-passthrough block.

```text
shmdt(0x7fe8b68f8000)           = 0
```

The "temporary-passthrough" statements in both evidence blocks describe the legacy code; at hermit f4055eb9cae4 the call is refused (Hermit design). No test file at that SHA names `shmdt`.
