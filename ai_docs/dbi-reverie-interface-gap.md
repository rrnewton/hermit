# DBI backend: the Reverie abstract-interface gap (why `--strict --backend dbi` fails)

Status: analysis + implementation spec. Author: agent `hermit-029`
(task `impl-dbi-scheduler`). Grounded in reverie rev
`e3e2c965e24b2a2287bac8b520caf7cd1b020d94` (the rev pinned by hermit) and the
hermit frontier at `344200e`.

## TL;DR

The task was originally framed as "implement a deterministic scheduler for the
DBI backend." **That framing is wrong.** Detcore *is* the deterministic
scheduler, and it is backend-agnostic: it is a Reverie `Tool`/`GlobalTool` pair
that already runs unchanged on any backend that implements Reverie's abstract
host contract. The ptrace backend implements that contract
(`reverie_ptrace::TracerBuilder::<Detcore>`); the DBI backend does not.

So the real work is **not** a new scheduler. It is making `reverie-dbi`
implement the same abstract Reverie backend interface that `reverie-ptrace`
implements, so that the *existing* Detcore can be hosted on the DynamoRIO
vehicle unchanged. `detcore` must not change and must not depend on
`reverie-dbi` (backend-abstraction commandment — currently honored: see
`detcore/Cargo.toml`, which depends only on `reverie` and, as a dev-dependency,
`reverie-ptrace`).

This document answers precisely **why `--strict --backend dbi` fails today** and
enumerates **exactly which Reverie host responsibilities `reverie-dbi` is
missing or stubs**, so that "L1 for the DBI backend" becomes *definable* (the
literal goal of the parent subtree).

## Why `--strict --backend dbi` fails — two independent layers

There is no clap-level conflict between `--strict` and `--backend dbi`
(`--strict` only `conflicts_with_all` the determinism opt-outs; see
`hermit-cli/src/bin/hermit/run.rs`). Two other things block it:

1. **The availability gate (surface symptom).**
   `Backend::unavailable_reason` in `hermit-cli/src/lib.rs:252-272` returns, for
   `Dbi`, either "the DynamoRIO SDK was not found ..." or (when the SDK is
   present) the unconditional "the DynamoRIO prototype does not yet expose a
   Detcore process launcher." `ensure_available` (called from
   `run.rs`) turns that into a hard error. This fires regardless of `--strict`.

2. **The real cause (root): the DBI path never runs Detcore at all.**
   `hermit-cli/src/bin/hermit/backends.rs::run_dbi` shells out to DynamoRIO's
   `drrun` with `reverie-dbi`'s prebuilt native client. That client hosts a
   hard-coded `PrototypeTool` — an in-C/in-Rust determinism policy — **not**
   Detcore. The ptrace path builds `TracerBuilder::<Detcore>` and returns the
   Detcore `GlobalState`; the DBI path returns only an OS exit code. So even if
   the gate in (1) were deleted, `--strict --backend dbi` would run the guest
   *without Detcore's scheduler, virtual time, or determinism report*. It would
   exit 0 while silently applying weaker guarantees than ptrace.

**Recommendation: do NOT delete the gate in (1) yet.** Removing it before
`reverie-dbi` can host Detcore would be *fail-open determinism* — `--strict`
would appear to succeed while not enforcing L1. That contradicts the project's
fail-closed policy (`docs/FAIL_CLOSED_STATUS.md`). The gate message is currently
*accurate*: the DBI prototype genuinely does not expose a Detcore launcher. The
gate should be lifted in the same change that makes the launcher real (see
"Definition of done").

## The abstract Reverie backend contract

There is no explicit `Backend`/`Tracer` trait in the `reverie` crate; the
contract is implicit. A backend that wants to host an arbitrary `Tool` `T`
(Detcore) must provide:

- **A per-thread guest handle** implementing `Guest<T>`
  (`reverie/src/guest.rs`) — which requires `GlobalRPC<T::GlobalState>`. Methods
  a Tool relies on: `memory`, `regs`, `stack`, `inject`, `tail_inject`,
  `set_timer`/`set_timer_precise`, `read_clock`, `thread_state[_mut]`, `tid`/
  `pid`/`ppid`, `daemonize`.
- **An async event driver** that reads `T::subscriptions(cfg)` and calls, over
  the guest's lifetime: `T::new`, `init_thread_state`, `handle_thread_start`,
  `handle_syscall_event`, `handle_cpuid_event`, `handle_rdtsc_event`,
  `handle_signal_event`, `handle_timer_event`, `handle_post_exec`,
  `on_exit_thread`, `on_exit_process`. Handlers are `async` and **may suspend**
  (Detcore blocks on scheduler turns).
- **A `GlobalState` owner + RPC transport** that constructs
  `T::GlobalState::init_global_state(cfg)` once and services
  `receive_rpc(tid, msg)` for every guest thread — including across address
  spaces if the backend runs guests in separate processes.
- **A spawn/wait surface** returning `(ExitStatus, T::GlobalState)` so the
  caller can print Detcore's determinism summary / schedule.

`reverie-ptrace` realizes all of this: `TracerBuilder<T: Tool + 'static>`
(`reverie-ptrace/src/tracer.rs`) with `new/config/gdbserver/spawn`, and
`TracedTask<T>` (`reverie-ptrace/src/task.rs`) driving the lifecycle on a tokio
`LocalSet`, implementing `Guest<T>`/`GlobalRPC` and returning the global state
from `wait()`/`wait_with_output()`. The ptrace backend is *centralized*: one
tracer process owns the single `GlobalState`.

## What `reverie-dbi` has today (rev e3e2c965)

- `DbiRunner` (`reverie-dbi/src/launcher.rs`): a subprocess wrapper around
  `drrun -disable_rseq -c <client.so> -- <guest>`. Returns
  `std::process::ExitStatus`/`Output` — **no `GlobalState`**. Disables ASLR via
  a `personality` `pre_exec`. This is a CLI wrapper, not a tool host.
- `DbiGuest<'a, T: Tool>` (`reverie-dbi/src/lib.rs:67-207`): **generic** and
  already implements `GlobalRPC<T::GlobalState>` and `Guest<T>`. Real:
  `memory` (in-process `LocalMemory`), `regs`, `inject`, `read_clock` (returns a
  branch counter). Stubbed: `tail_inject` panics (`lib.rs:193`), `stack`
  panics (`lib.rs:231,235`), `set_timer`/`set_timer_precise` return `ENOSYS`
  (`lib.rs:196-202`), `daemonize` no-op.
- `PrototypeTool` (`reverie-dbi/src/lib.rs`): a hard-coded `Tool` with
  `GlobalState = ()`, implementing only `handle_syscall_event`.
- The C-ABI driver (`reverie-dbi/src/lib.rs:648-747` + `native/client.c`):
  binds `static PROTOTYPE_TOOL: PrototypeTool`, `static GLOBAL_STATE: () = ()`,
  `static CONFIG: () = ()` (`lib.rs:658-660`); on each syscall constructs a
  `DbiGuest` and calls `run_ready(PROTOTYPE_TOOL.handle_syscall_event(...))`
  (`lib.rs:733`). `run_ready` polls the future **once** with a noop waker and
  `panic!("the prototype tool handler must not suspend")` on `Poll::Pending`
  (`lib.rs:654`). CPUID, virtual clocks and rlimits are emulated in C before the
  tool ever sees a syscall.

Net: `DbiGuest<T>` is generic, but nothing constructs `DbiGuest<Detcore>` or
dispatches Detcore's handlers, and the driver structurally cannot host a Tool
whose handlers suspend.

## The precise gap list

### Fundamentally missing (architecture, not a stub)

1. **Generic tool hosting at the dispatch site.** The native callback binds a
   concrete `static PrototypeTool`. Needed: a `DbiTracer<T>`/`TracerBuilder<T>`
   equivalent that instantiates `T`, `T::GlobalState`, `T::Config` and is
   reachable from the C ABI (monomorphized entry points or a type-erased
   registration).
2. **An async executor that permits suspension.** `run_ready` panics on
   `Poll::Pending`. Detcore's `handle_syscall_event`/`handle_timer_event` block
   on scheduler turns. Requires a real (single-threaded / `LocalSet`) executor,
   mirroring ptrace's `cancellable(...)` model.
3. **A `GlobalRPC` transport that can block and cross address spaces.** DynamoRIO
   instruments each guest process in its own address space; Detcore's single
   `GlobalState` (the scheduler) must be reachable from every guest thread. The
   in-process `send_rpc → receive_rpc` shortcut only works for a `()` global in
   one address space. This is the crux: it needs either a centralized host
   process with shared-memory/socket RPC + `bincode` serialization (cf.
   `WrappedFrom` in `task.rs`), or a fundamentally centralized DBI driver.
4. **Timer / RCB-preemption path.** `set_timer*` return `ENOSYS`; there is no
   post-branch threshold trap and no `handle_timer_event` dispatch. The C client
   maintains a free-running `branch_count` *sampled at syscall boundaries*
   (`native/client.c` via `drx_insert_counter_update`), but nothing compares it
   to a target to fire a preemption event. Detcore's deterministic preemption
   depends on this (`TimerSchedule::Rcbs`, `reverie/src/timer.rs`).
5. **Signal event delivery.** The client registers no signal event; Detcore's
   `handle_signal_event` is never invoked.
6. **Clone/fork/exec/thread lifecycle.** No equivalent of
   `TracedTask::cloned/forked/handle_new_task/handle_exec_event`.
   `init_thread_state`/`handle_thread_start`/`handle_post_exec` are never called;
   `ppid` is always `None`. Detcore builds a per-thread/per-process tree.
7. **Exit callbacks + global-state return.** `on_exit_thread`/`on_exit_process`
   never fire; there is no `wait() -> (ExitStatus, GlobalState)`. Detcore's
   determinism report/schedule is never surfaced.
8. **Subscription wiring.** `T::subscriptions` is never consulted; the client
   filters all syscalls and hardwires CPUID. Needed: translate a `Subscription`
   into DynamoRIO instrumentation choices.

### Merely stubbed (structurally present; smaller once the above exists)

9. `DbiGuest::tail_inject` panic (`lib.rs:193`).
10. `DbiGuest::stack`/`UnsupportedStack` panic/ENOSYS (`lib.rs:210-241`).
11. `set_timer`/`set_timer_precise` ENOSYS (`lib.rs:196-202`) — becomes real
    once (4) exists.
12. `config()` returns `&static ()` (`lib.rs:660`) — needs real `T::Config`.

## Definition of done — "L1 for the DBI backend"

L1 = the DBI backend hosts *Detcore* (not `PrototypeTool`) and enforces the same
deterministic-scheduling guarantees ptrace does, verified by:

- `hermit run --strict --backend dbi -- <prog>` exits 0 for a set of guests and
  produces a Detcore `GlobalState`/determinism summary (not just an OS exit
  code), i.e. `run_dbi` returns via a `DbiTracer::<Detcore>::spawn().wait()`
  surface analogous to `run_with_backend` for ptrace.
- The same guest run twice under `--backend dbi` yields identical Detcore
  schedules (determinism), and matches the ptrace schedule for
  scheduler-observable events where the vehicles agree.
- `detcore` is unchanged and still has no `reverie-dbi` dependency.
- The availability gate (`Backend::unavailable_reason` for `Dbi`) is lifted in
  the *same* change, so it never advertises L1 before the launcher is real.

## Phased roadmap (all in `reverie-dbi` + `hermit-cli`, never `detcore`)

- **P0 (done / documented):** source-built DynamoRIO recipe unblocks the drrun
  vehicle; `PrototypeTool` hello-world runs (`experiments/hello/README.md`).
- **P1a — centralized host skeleton:** add `DbiTracer<T>`/builder that owns
  `T::GlobalState` + a `LocalSet` executor; replace `run_ready`'s one-shot poll
  with a real driver; construct `DbiGuest<T>`; dispatch `handle_syscall_event`
  for a *single-threaded* guest with a `()`-or-real global. Return
  `(ExitStatus, GlobalState)`.
- **P1b — cross-process GlobalRPC:** shared-memory/socket transport so a guest
  in DynamoRIO's address space reaches the central `GlobalState`; serialize
  requests/responses. Wire `Subscription` → instrumentation.
- **P1c — scheduler-critical events:** RCB threshold trap → `handle_timer_event`
  + real `set_timer`; signal delivery → `handle_signal_event`; `tail_inject` and
  guest `stack`. This is what actually lets Detcore *schedule* on DBI.
- **P2 — lifecycle:** clone/fork/exec/thread-start/exit callbacks; multi-thread
  scheduling parity with ptrace.
- **P3 — parity harness:** run the xfail matrix under `--backend dbi`, gated on
  DBI availability, and diff Detcore schedules dbi-vs-ptrace.

## Practical/build notes

- `reverie-dbi` is a pinned git dependency (rev `e3e2c965`). Landing changes to
  it requires modifying the reverie repo and re-pinning (or a local `[patch]`/
  path override for development). Cross-repo, so it cannot land as a hermit-only
  diff.
- Building `reverie-dbi` compiles DynamoRIO from a submodule (heavy). A
  **prebuilt** DynamoRIO release fails at runtime (`<ERROR: using undefined
  symbol!>`); a **source build of DynamoRIO main** is required
  (`experiments/hello/README.md`). A working client is present in this
  environment at `/tmp/reverie-dbi-69f-target/reverie-dbi-native/libreverie_dbi_client.so`.
- The xfail harness must be gated on DBI availability (SDK + client) and skip
  cleanly (not xfail) when absent, so CI on hosts without DynamoRIO stays green.
