---
schema: syscall-pages/v1
syscall: epoll_create
nr_x86_64: 213
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: true
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_create&body=Discussion+for+the+%60epoll_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_create%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`epoll_create(size)` creates a new epoll instance and returns a file descriptor for it. This is the older form of `epoll_create1`. Detcore converts the call to `epoll_create1(0)` and runs it through the shared `handle_epoll_create1` handler (`detcore/src/lib.rs:2618`-`2621` at hermit f4055eb9cae4). That handler lets the kernel create the instance and records the new descriptor in Detcore's fd table. One known gap: the conversion drops `size`, so `epoll_create(0)` and `epoll_create(-1)` succeed under Hermit, where Linux returns `EINVAL`.

## Linux semantics

`size` was once a hint for how many descriptors the instance would watch. Since Linux 2.6.8 the kernel ignores the value, but it must still be greater than zero for backward compatibility. Otherwise the call fails with `EINVAL`.

On success the call returns the lowest free descriptor number in the calling process. The descriptor does not have close-on-exec set; only `epoll_create1(EPOLL_CLOEXEC)` sets it. Other errors:

- `EMFILE`: the process has reached `RLIMIT_NOFILE`.
- `ENFILE`: the system-wide limit on open files has been reached.
- `ENOMEM`: the kernel could not allocate the instance.

The instance lives until every descriptor that refers to it is closed. `fork` and `dup` share it, and so does `execve`, because close-on-exec is not set.

Host probe on the agent host (Linux 7.1.3, python ctypes, 2026-10-03), recorded by this body's author: `epoll_create(0)` and `epoll_create(-1)` gave `EINVAL`, and `epoll_create(1)` returned fd 3.

### Flags and options

- F1: `size` greater than zero: the value is ignored, a new epoll instance is created, and a descriptor without close-on-exec is returned (the same as `epoll_create1(0)`)
- F2: `size` zero or negative: `EINVAL`, and no descriptor is created
- F3: descriptor allocation: the lowest free descriptor number is used, and `EMFILE` is returned at `RLIMIT_NOFILE`
- F4: host resource failure: `ENFILE` at the system-wide file limit, `ENOMEM` when allocation fails
- F5: lifetime: without close-on-exec, the descriptor is shared across `fork` and `dup` and survives `execve`

## Determinism analysis

`linux_determinism: conditional`. When it succeeds, the result is a function of the process's own state: whether `size` is valid (F1, F2), and the lowest free descriptor number (F3). A program that opens and closes descriptors in the same order gets the same number each time.

### Nondeterminism sources

- `RLIMIT_NOFILE` is inherited from whatever launched the process, so the point at which `EMFILE` appears depends on the launcher (F3).
- `ENFILE` and `ENOMEM` depend on host-wide state outside the process (F4).
- Descriptors the process inherits at startup change which number is lowest and free. Hermit's launcher decides this, not the guest.

The epoll instance itself holds no host-dependent data when it is created. What it later reports through `epoll_wait` belongs to `epoll_ctl` and `epoll_wait`, not to this call.

### Cross-host variation

- Host `RLIMIT_NOFILE` inherited by Hermit's guest: the kernel enforces that limit, not the virtual 1048576 that Detcore reports (`detcore/src/tool_local.rs:289`), so the descriptor count at which F3's `EMFILE` appears differs between hosts.

## Hermit design

Target policy: `model`. The kernel creates the real instance, and Detcore keeps its own record of the descriptor so that later fd operations work.

What happens today:

- The typed arm converts `EpollCreate` to `EpollCreate1` (`detcore/src/lib.rs:2618`-`2621`). In the pinned Reverie (`reverie-syscalls/src/syscalls.rs:3107`-`3116`, Reverie d766df20b780), `impl From<EpollCreate> for EpollCreate1` sets the flags argument to 0 and drops `size`. Its doc comment admits that a negative `size` changes behaviour after the conversion.
- `handle_epoll_create1` (`detcore/src/syscalls/io.rs:1143`) makes an empty resource request to the scheduler (`detcore/src/syscalls/io.rs:1149`).
- It then hands the call to the record/replay layer (`detcore/src/syscalls/io.rs:1150`; `record_or_replay` at `detcore/src/tool_local.rs:382`), which runs the syscall on the host. The kernel picks the descriptor number from the guest process's own descriptor table (F3).
- It registers the descriptor in Detcore's fd table as `FdType::Epoll`, with close-on-exec taken from the flags (`detcore/src/syscalls/io.rs:1161`-`1166`). The flags are 0 after the conversion, so the descriptor is recorded without close-on-exec, as F1 and F5 require. The comment above this call explains that `dup` and `fcntl` on the epoll descriptor fail with `EBADF` without the registration.
- The syscall is subscribed (`detcore/src/lib.rs:1205`) and is in the `Determinized` list (`detcore/src/syscall_classification.rs:72`).

Gap against Linux:

- **F2 is not modeled.** Because `size` is dropped before the kernel sees it, `epoll_create(0)` and `epoll_create(-1)` create an instance and return a descriptor under Hermit. Linux returns `EINVAL`. The answer is the same on every run, but it is the wrong answer. Fix: in the `EpollCreate` arm, return `EINVAL` when `size <= 0`, before the conversion. This needs a hermit issue; this agent had no GitHub access and could not file one.

F4 is passed through from the host. An `ENFILE` or `ENOMEM` is a host failure that Hermit cannot make deterministic, the same as for any other descriptor-creating call.

`model` fits better than the alternatives:

- `passthrough` would leave the descriptor out of Detcore's fd table and break later fd operations.
- `emulate` cannot work, because later epoll calls need a real kernel instance.
- `refuse` would break programs that still use the old entry point, including older libevent and libuv builds.

## Recommended tests

- T1 (covers F1, F5): `epoll_create(1)` and `epoll_create(1024)` each return a descriptor. `fcntl(F_GETFD)` shows close-on-exec clear, `dup` of the descriptor succeeds, and an `epoll_ctl` add of a pipe followed by `epoll_wait` reports the pipe once it is written. A child created with `fork` and `execve` still sees the descriptor open. All of this runs under `hermit run --verify` on ptrace.
- T2 (covers F2): `epoll_create(0)` and `epoll_create(-1)` must return -1 with `errno` `EINVAL` and leave the descriptor table unchanged. This fails at f4055eb9cae4, where both calls succeed. Add it together with the fix above.
- T3 (covers F3): close descriptors 3-5, then call `epoll_create(1)` three times. The calls must return 3, 4 and 5 in that order. Then lower `RLIMIT_NOFILE` to the current descriptor count and check that the next call returns `EMFILE`. Under `--verify`, the numbers must be identical across runs.
- T4 (covers none): `ENFILE` and `ENOMEM` cannot be triggered reliably without host-wide side effects. With a fault-injection hook in the test harness, assert that a failed kernel call registers no descriptor. It no longer claims F4, which U1 records as a host-transient error.
- U1 (covers F4; host-transient): `ENFILE` and `ENOMEM` depend on host-wide state shared with other processes. The host call at `detcore/src/syscalls/io.rs:1150` returns the error to the guest unchanged today. F4 counts toward done only once Hermit hides these errors or reports them as an infrastructure error.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (converted to epoll_create1(0), kernel creates the instance, descriptor registered as FdType::Epoll; lib.rs:2618, io.rs:1143); linux_determinism stays conditional (RLIMIT_NOFILE, ENFILE/ENOMEM, inherited descriptors); 5 flags and 4 tests listed; tracking_status stays todo: no test names this syscall, and F2 is a defect (size <= 0 succeeds instead of EINVAL) that needs a hermit issue
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d12 (owner decisions 1 and 2): cross_host_variant true (the kernel enforces the host RLIMIT_NOFILE, not the virtual one, so the EMFILE point of F3 differs between hosts); F4 ENFILE/ENOMEM recorded as host-transient U1 and T4 no longer claims F4; no open questions


- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `epoll_create`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+epoll_create&body=Discussion+for+the+%60epoll_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fepoll_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60epoll_create%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `epoll_create`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The Reverie typed struct is `EpollCreate { size: i32 }` (`reverie-syscalls/src/syscalls.rs:2190` at Reverie d766df20b780). Its doc comment says `size` "must be greater than 0", which the conversion at lines 3107-3116 does not enforce.

No test at f4055eb9cae4 names `epoll_create`; `code_facts.tests_naming` is 0. `tests/c/epoll_determinism.c` creates its epoll instance with `epoll_create1(EPOLL_CLOEXEC)` (`tests/c/epoll_determinism.c:39`).
