---
schema: syscall-pages/v1
syscall: get_robust_list
nr_x86_64: 274
linux_determinism: conditional
target_policy: passthrough
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/371
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+get_robust_list&body=Discussion+for+the+%60get_robust_list%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fget_robust_list.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60get_robust_list%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: passthrough
  dispatch: "passthrough"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: false
  tests_naming: 8
---

## Summary

`get_robust_list(pid, head_ptr, len_ptr)` returns the robust-futex list head that a thread registered with `set_robust_list` (glibc registers one for every thread it starts), and the size of that structure. Hermit forwards it to the host kernel unchanged. The thread ids it takes are ids in the guest's own PID namespace, and the head it returns is a guest address the guest registered itself, so the answer repeats when the guest's thread creation order and address layout repeat.

## Linux semantics

The kernel looks up the target under RCU. `pid = 0` means the calling thread. Any other value is a thread id looked up in the caller's PID namespace; it may name any thread of any process there, not only the caller's own thread group. For a thread other than the caller, the kernel requires `ptrace_may_access(task, PTRACE_MODE_READ_REALCREDS)`: the caller's real credentials must match the target's, or the caller must hold `CAP_SYS_PTRACE` in the target's user namespace, and a non-dumpable target needs that capability. Yama's `ptrace_scope` restricts only attach-mode access, so it does not apply. On success the kernel writes `sizeof(struct robust_list_head)` (24 on x86_64) to `*len_ptr` first, then the target's stored head pointer to `*head_ptr`, and returns 0. It reads nothing from the target's memory: the head is the pointer value the target registered, not the list it points to. A thread that never registered, a program after `execve` before its libc registers, and a thread whose list was already released at exit all report a null head. Nothing in the caller or the target changes.

Manual reference: Linux man-pages get_robust_list(2).

### Flags and options

- F1: `pid = 0`: the calling thread's own registration
- F2: `pid` is the thread id of another thread in the caller's thread group
- F3: `pid` is a thread id in another process in the caller's PID namespace, subject to the access check in F6
- F4: the target has no registration: a raw `clone` thread or a non-glibc program that never called `set_robust_list`, a program after `execve` before its libc registers, a thread that registered a null head, or a zombie whose list was released at exit. The call succeeds and reports a null head.
- F5: no thread with that id exists in the caller's PID namespace, including negative values, reaped children and ids outside the namespace: `ESRCH`
- F6: the target is another thread and `ptrace_may_access(PTRACE_MODE_READ_REALCREDS)` fails (different real credentials and no `CAP_SYS_PTRACE` over the target's user namespace, or a non-dumpable target without that capability): `EPERM`
- F7: outputs: `*len_ptr` is always 24 on x86_64, whatever the target registered with; `*head_ptr` is exactly the pointer the target last registered, so a `set_robust_list(&h, 24)` followed by `get_robust_list(0, ...)` returns `&h`
- F8: `len_ptr` outside the address space: `EFAULT` with nothing written. `head_ptr` outside the address space: `EFAULT`, after `*len_ptr` has already been written.

## Determinism analysis

`conditional`. The call reads only two inputs: which thread `pid` names, and the pointer that thread registered.

- Thread ids are ids in the guest's fresh PID namespace. Detcore's constant `ROOT_DETPID` rests on "a convention of how we set up our PID namespace" that starts the guest at pid 3 (`detcore/src/consts.rs:29` and `:30`). `gettid` is passed through (`detcore/src/syscall_classification.rs:705`), so the ids a guest learns are namespace ids, and the comment on `handle_tgkill` at `detcore/src/syscalls/signal.rs:673` and `:674` relies on the same fact: "Guest PID/TID values are stable in the fresh PID namespace". Thread and process creation is serialized by Hermit's scheduler, so the namespace hands out the same ids in the same order on every run. `get_robust_list` therefore resolves the same target on every run, and `ESRCH` (F5) repeats with it.
- The head is a guest virtual address: glibc's per-thread `robust_head` inside the thread descriptor, or whatever the guest passed to `set_robust_list`. It repeats exactly when the guest's address layout repeats. The classification's own review marker names this assumption: "Review process-local robust-list queries under fixed guest address and PID namespaces" (`detcore/src/syscall_classification.rs:708` and `:709`). The code that fixes the guest's address layout is in the launcher, which is not in the exported tree, so this body cannot confirm the layout is fixed; see the open question. The dependency is not specific to this syscall: any guest that prints the address of its own stack or thread data has the same one.
- The access check (F6) compares credentials that start from the fixed container identity, so it repeats too.

Nothing in the call depends on timing. Under Hermit a query racing the target's own `set_robust_list` or exit lands in a fixed order.

### Nondeterminism sources

- The guest's address layout, which decides the head value in F7. Whether Hermit holds it fixed is not visible in the exported tree.
- Raw pid identity and target lifetime: fixed under Hermit by the fresh PID namespace and serialized creation and exit, as above. On a bare host, pids are not repeatable.
- Credential and ptrace checks (F6): fixed by the container identity under Hermit.
- Concurrent robust-list registration by the target thread: serialized by Hermit's scheduler.

## Hermit design

Target policy: `passthrough` (legacy NEEDS-HANDLER, not adopted). The code already does this at hermit f4055eb9cae4, and it has been reviewed as such:

- `get_robust_list` is classified `PassThrough` at `detcore/src/syscall_classification.rs:710`, next to `set_robust_list` at `:711`, inside the pass-through block that starts at `detcore/src/syscall_classification.rs:690`, whose comment at `:691` and `:692` gives the condition ("conditionally repeatable under Hermit's fixed-container, stable-filesystem, and serialization assumptions"). The review marker at `detcore/src/syscall_classification.rs:708` and `:709` (`TODO-HUMAN-REVIEW(PR-889)`) scopes the review to fixed guest address and PID namespaces.
- The unit test `get_robust_list_is_a_reviewed_passthrough` (`detcore/src/syscall_classification.rs:2197`) asserts that both `get_robust_list` and `set_robust_list` are `PassThrough`. The census test `census_refuses_a_count_preserving_swap` (`detcore/src/syscall_classification.rs:1859`) uses `get_robust_list` at `:1866` as its example of a reclassification the census must catch.
- The generic arm `SyscallClassification::PassThrough => self.passthrough(guest, call).await` at `detcore/src/lib.rs:2843` forwards it. `passthrough` at `detcore/src/lib.rs:367` only calls `record_or_replay_preserving_tool_errors` (`detcore/src/tool_local.rs:349`). The injecting `NoopTool` passes the call through unchanged (`detcore/src/record_or_replay.rs:47`).

The legacy body recommended NEEDS-HANDLER because "non-self queries use raw PIDs, permissions, target lifetime, and another task pointer". Each of those is already fixed without a handler:

- Raw PIDs: the guest runs in its own PID namespace and only ever holds namespace ids, which Hermit reproduces by serializing thread and process creation. A handler that translated ids would have nothing to translate.
- Target lifetime: exits and reaping are serialized, so whether a target is live, a zombie (F4) or gone (F5) repeats.
- Permissions: the credentials come from the fixed container identity.
- Another task's pointer: the value is the target's own registration, a guest address. Replacing it would break the contract with `set_robust_list` (F7) that `tests/c/robust_list.c:93` checks, and the kernel itself walks the real registered list when the thread exits; the value must stay the real one.

Emulating the call (answering from a Detcore copy of each thread's registration) would mean also handling `set_robust_list` (https://github.com/rrnewton/hermit/issues/1953) and `execve` resets, while the kernel keeps the authoritative copy it needs for owner-death cleanup. That duplicates state with no gain. Refusing it would break glibc programs that inspect their robust list, such as the owner-death fixture below.

Record and replay. Code facts show `subscribed_record_replay: false`, so Detcore's `--passthru-opt` subscription leaves `get_robust_list` out. `hermit record` has its own subscriptions, which the comment at `detcore/src/lib.rs:1078` places in a recorder module not in the exported tree, so this body does not describe how record captures `get_robust_list`. No record/replay integration test issues it at this SHA.

Existing run-mode evidence, on ptrace:

- The cargo integration test `robust_list_queries_verify` (`hermit-cli/tests/robust_list_queries.rs:30`) builds three guests (`hermit-cli/tests/robust_list_queries.rs:37` to `:44`) and runs each under `--backend=ptrace run --strict --verify` (`hermit-cli/tests/robust_list_queries.rs:60` to `:67`):
  - `tests/c/get_robust_list_self.c:20` queries `pid = 0` and checks a non-null head and a length equal to the structure size (`tests/c/get_robust_list_self.c:24`).
  - `tests/c/get_robust_list_thread.c:44` queries a sibling thread's id and checks the same (`tests/c/get_robust_list_thread.c:48`).
  - `tests/c/get_robust_list_child.c:48` queries a forked child and checks only the length (`tests/c/get_robust_list_child.c:52`).
  The same three guests have verify cells with ptrace in CI: `c-programs/get-robust-list-self` (`tests/e2e/manifests/c-programs.yaml:1795`), `c-programs/get-robust-list-thread` (`:1859`) and `c-programs/get-robust-list-child` (`:1728`).
- `tests/c/robust_list.c` queries itself (`:46`), registers its own head with `set_robust_list` (`:64`), queries again (`:80`), and checks that the head equals the registered address (`:93`) and the length equals its size (`:104`). Cell `c-programs/robust-list` at `tests/e2e/manifests/c-programs.yaml:13532` runs it in verify mode with `ci: true`.
- Incidental: `tests/bin/robust_futex_test.c:69` queries the owner thread's registration (cell `bin-c/robust-futex-test` at `tests/e2e/manifests/bin-c.yaml:62`, ptrace disabled in CI as an infrastructure error with no comparison recorded), and `tests/c/dbt_unsupported_syscall.c:31` calls it in a fixture whose ptrace cell (`tests/e2e/manifests/c-programs.yaml:1177`) is unavailable because the fixture invokes `restart_syscall`.

None of these prints the head value, so none shows whether it repeats across runs. No coverage token is recorded yet: the existing tests cover F1, F2, F7 and the length half of F3, and F4, F5, F6 and F8 have no test.

## Recommended tests

- T1 (covers F1, F7): under `hermit run --verify` on ptrace, query `pid = 0`, register a head with `set_robust_list(&h, 24)`, query again, and assert the head is `&h` and the length is 24. Existing coverage: `tests/c/robust_list.c:46` to `:104` (cell `c-programs/robust-list`) and `tests/c/get_robust_list_self.c:20` (driven by `hermit-cli/tests/robust_list_queries.rs:30`).
- T2 (covers F2, F7): query a sibling thread by its `gettid` value and assert the head equals the address that thread reports for its own registration (read in the thread with `pid = 0` and passed back), and the length is 24. Existing partial coverage: `tests/c/get_robust_list_thread.c:44` (non-null head and length only).
- T3 (covers F3, F7): query a forked child that sends its own `pid = 0` head over a pipe, and assert the parent sees the same head and length 24. Existing partial coverage: `tests/c/get_robust_list_child.c:48` (length only).
- T4 (covers F4): assert a null head for a thread that registered a null head with `set_robust_list(NULL, 24)`, for a raw `clone` thread that never registered, and for an exited but unreaped child.
- T5 (covers F5): assert `ESRCH` for `pid = -1`, for a reaped child's id and for an id above `/proc/sys/kernel/pid_max`.
- T6 (covers F6): fork a child that drops to a different mapped uid, or makes itself non-dumpable with `prctl(PR_SET_DUMPABLE, 0)` and drops `CAP_SYS_PTRACE`, and assert `EPERM` from the parent's query, or `EPERM` from a child that drops its own capabilities and queries a parent with different credentials. Under ptrace the guest is root in its user namespace and holds `CAP_SYS_PTRACE` there, so the test must state which arrangement reaches `EPERM` in the ptrace CI lane, or record that none does.
- T7 (covers F8): assert `EFAULT` for `len_ptr = (size_t *)1` with `*head_ptr` unchanged, and `EFAULT` for `head_ptr = (void **)1` with `*len_ptr` already set to 24.
- T8 (covers F1, F2, F7): print the raw head values from the self and thread queries to stdout under `hermit run --verify`, so that the comparison shows whether the guest address layout, and with it the head value, repeats across runs. This answers the open question below.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/371: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: passthrough, dispatch passthrough.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: set conditional/passthrough (target_policy investigate -> passthrough; legacy NEEDS-HANDLER not adopted: classified at detcore/src/syscall_classification.rs:710 and pinned by get_robust_list_is_a_reviewed_passthrough at :2197, forwarded at detcore/src/lib.rs:2843; guest tids are PID-namespace ids that need no translation, and the head is the guest's own registration); listed 8 flag behaviours and 8 tests; related now names set_robust_list; no coverage token recorded because F4, F5, F6 and F8 have no test, so tracking_status stays todo; open questions for the owner on whether the guest address layout (the head value) is fixed and on reaching EPERM under ptrace
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant stays false (the length is the fixed x86-64 ABI size and the head is the guest's own registration); no U items or splits, because F6's EPERM is decided by the guest's and container's credentials (class A) and stays T6 coverage; the open question on reaching EPERM under ptrace is resolved by decision 1, and the address-layout question stays open
- 2026-10-05 · hermit f4055eb9cae4 · syscall-tracking-2 · covered: T1 covered by tests/c/robust_list.c (C08 review of the 2026-10-04 coverage proposals; guest source read at this SHA, not run by this agent)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `get_robust_list`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+get_robust_list&body=Discussion+for+the+%60get_robust_list%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fget_robust_list.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60get_robust_list%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: get_robust_list" (https://github.com/rrnewton/hermit/issues/371), legacy variant A2.

Legacy text replaced in this pass: Summary "Rank 92 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls." (a sweep ordering, not a property of the syscall). Determinism analysis "Self queries reflect guest-registered state, but non-self queries use raw PIDs, permissions, target lifetime, and another task pointer." Kept above and answered under Hermit design. Legacy recommended action NEEDS-HANDLER, not adopted (see Hermit design).

### Open questions

- F7: does Hermit fix the guest's address layout (for example by disabling address-space randomization for the guest), so that the head value repeats across runs? The review marker at `detcore/src/syscall_classification.rs:708` assumes "fixed guest address" namespaces, but the launcher code is not in the exported tree. T8 measures it. If the layout is not fixed, the head value differs between runs exactly as any printed guest pointer does; the owner decides whether that belongs to this issue or to a general address-layout issue.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=get_robust_list -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(274, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
get_robust_list(-1, 0xffffffffffffffff, 0xffffffffffffffff) = -1 ESRCH (No such process)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### 2026-10-03 host check

On the measurement host (Linux 7.1.3, glibc 2.34, x86_64): `get_robust_list(0, &head, &len)` returned `get_robust_list(0, [0x7fc897a07a20], [24]) = 0`, and `get_robust_list(0, &head, (size_t *)1)` returned `EFAULT`.
