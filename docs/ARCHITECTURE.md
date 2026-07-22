# Hermit Architecture

Hermit makes Linux program execution more reproducible by running a guest under
the Reverie tracing framework and handling nondeterministic events in Detcore.
This document describes the current ptrace backend: where state lives, how an
event travels from the guest to Detcore and back, and how scheduling, resources,
time, and chaos mode fit together.

The most important boundary is:

- **Reverie controls execution.** It owns ptrace, seccomp, tracee lifecycle,
  register and memory access, syscall injection, and event delivery.
- **Detcore defines policy.** It decides which events to emulate, transform,
  serialize, record, replay, or pass through to Linux.
- **Linux still implements most operations.** Detcore virtualizes selected
  results and orders selected effects; it is not a replacement kernel.

## System overview

Hermit has three layers plus the guest and host kernel:

```text
 hermit CLI
   parses mode/configuration
   prepares container namespaces, mounts, affinity, environment
          |
          v
 Reverie ptrace backend (tracer process)
   TracerBuilder<Detcore<_>>
   one tracing task per guest TID
   ptrace event routing, registers, memory, syscall injection
          |
          +---------------- Detcore policy ----------------+
          |                                                |
          | one Detcore instance per guest process         |
          | one ThreadState per guest thread               |
          | one GlobalState for the complete process tree  |
          | scheduler, resource protocol, logical time     |
          +------------------------------------------------+
          |
          | ptrace stops/resumes and injected syscalls
          v
 Linux kernel
   seccomp filter, process/thread creation, signals, real syscalls
          ^
          |
 guest process tree
   application threads and child processes
```

All Detcore global state is central in the tracer address space when using the
ptrace backend. Guest processes do not link Detcore and cannot directly mutate
its state. They communicate with it only by reaching traced events.

## Startup and lifetime

The CLI builds a `Config`, validates incompatible settings, and prepares the
guest command. Normal containerized operation also establishes the requested
PID namespace and other container settings before the tracing loop begins.
Thread pinning may constrain the container to a selected CPU.

`hermit::run` creates `TracerBuilder<Detcore<_>>` and supplies the Detcore
configuration. Reverie then performs the following initialization:

1. Initialize the single `GlobalState` through the `GlobalTool` interface.
2. Ask Detcore for its event `Subscription`.
3. Resolve and spawn the guest program under `PTRACE_TRACEME`.
4. Install a seccomp-BPF filter derived from the subscription.
5. Set ptrace options for exec, clone, fork, vfork, exit, and seccomp events.
6. Initialize the root process-level `Detcore` and root `ThreadState`.
7. Start the tracing task tree and the Detcore scheduler task.

The top-level run uses a current-thread Tokio runtime. This avoids allowing an
executor's worker-thread count and worker TIDs to become an accidental source
of process allocation nondeterminism.

When the root tracee terminates, Reverie waits for the traced process tree and
returns its exit status. Hermit then explicitly cleans up `GlobalState`, joins
the scheduler, and reports execution statistics.

## State ownership

Reverie's `Tool` abstraction separates state at three scopes. Detcore uses all
three:

```text
                         complete traced tree
                 +--------------------------------+
                 | GlobalState                    |
                 | - deterministic scheduler      |
                 | - aggregate GlobalTime         |
                 | - inode and port allocation    |
                 | - alarms and replay data       |
                 +---------------+----------------+
                                 ^ RPC
                 +---------------+----------------+
                 |                                |
       guest process A                  guest process B
 +-------------------------+      +-------------------------+
 | Detcore process state   |      | Detcore process state   |
 | - DetPid                |      | - DetPid                |
 | - immutable Config      |      | - immutable Config      |
 | - record/replay policy  |      | - record/replay policy  |
 +------------+------------+      +------------+------------+
              |                                |
       +------+------+                   +-----+------+
       |             |                   |            |
 ThreadState A1  ThreadState A2     ThreadState B1   ...
```

### Process state

A `Detcore<T>` exists for each guest process. It stores the deterministic
process ID, a configuration snapshot, and the selected record/replay sub-tool.
Detcore acts as a filter in front of that sub-tool: fully deterministic events
can be emulated locally, partially deterministic events can be injected and
then rewritten, and irreducibly external events can be delegated for recording
or replay.

### Thread state

Each guest thread has a serializable `ThreadState`. Its important fields are:

- deterministic TID/PID and a deterministic ancestry pedigree;
- syscall, signal, and timeslice statistics;
- the per-thread `DetTime` and last committed hardware RCB counter;
- current timeslice end, timer state, and recorded preemption points;
- deterministic application and chaos PRNGs;
- file descriptor metadata, shared or copied according to `CLONE_FILES`;
- record/replay thread state.

For a new thread, Reverie snapshots the parent thread state at the clone event.
Detcore then fixes up the child-specific identity, derives child PRNG streams
from the parent, inherits the parent's logical time, and gates the child's first
instruction on a scheduler start request.

### Global state

`GlobalState` is shared by the whole traced tree. RPC requests carry the
caller's current `DetTime`; the receiver updates the corresponding component of
`GlobalTime` before processing the operation. Global state owns the scheduler,
deterministic inode metadata, port allocation, alarms, and schedule/preemption
replay inputs.

This split is intentional: a thread can cheaply update state that only it owns,
while operations requiring a total order cross the RPC boundary.

## Reverie interception pipeline

### Seccomp selects events; ptrace delivers them

Detcore's subscription controls which syscalls are traced. Reverie translates
that subscription into a seccomp filter:

- subscribed syscalls return `SECCOMP_RET_TRACE`;
- other syscalls are allowed without a ptrace syscall stop;
- `restart_syscall` and `rt_sigreturn` are always allowed;
- syscalls executed from Reverie's injection trampoline are allowed so an
  injected syscall does not recursively trap itself.

Debug builds subscribe to all events. Optimized builds enumerate the syscalls
Detcore handles and optionally subscribe to CPUID and RDTSC fault events.
Consequently, adding a handler is not sufficient in an optimized build: its
event must also be present in `Detcore::subscriptions`.

The tracer enables these ptrace options:

```text
 PTRACE_O_TRACEEXEC       PTRACE_O_TRACECLONE
 PTRACE_O_TRACEFORK       PTRACE_O_TRACEVFORK
 PTRACE_O_TRACEVFORKDONE  PTRACE_O_TRACEEXIT
 PTRACE_O_TRACESECCOMP    PTRACE_O_TRACESYSGOOD
 PTRACE_O_EXITKILL
```

`PTRACE_O_EXITKILL` prevents a guest from escaping if the tracer dies.

### One tracing task per guest thread

The ptrace backend maintains an asynchronous tracing task for every guest TID.
At a stop, that task owns the stopped tracee, reads its state, awaits the tool
handler, and finally chooses how to resume it. Fork and clone events create new
tasks; exec changes the process image while preserving the appropriate tool
state; exit tears down the task and invokes Detcore's exit hook.

Although the tracing tasks are asynchronous, ptrace operations on a given
tracee occur only while that tracee is stopped. Detcore's scheduler adds the
stronger cross-thread ordering used in sequentialized modes.

## End-to-end syscall path

For a subscribed syscall, the complete path is:

```text
 Guest thread        Linux/seccomp       Reverie task        Detcore
      |                    |                   |                 |
  1. syscall ------------>|                   |                 |
      |              RET_TRACE stop --------->|                 |
      |                    |              2. read registers     |
      |                    |              3. decode Syscall --->|
      |                    |                   |            pre-handler
      |                    |                   |            dispatch
      |                    |                   |       [scheduler RPC]
      |                    |                   |       emulate/inject
      |                    |<------ 4. injected syscall --------|
      |                    |------ kernel result -------------->|
      |                    |                   |            post-handler
      |                    |                   |<-- Result<i64> --|
      |                    |              5. suppress original
      |                    |                 if still pending
      |                    |              6. write return value
      |<-------------------|<------------- 7. resume (+ signal)
```

More precisely:

1. The seccomp filter stops the tracee before the original syscall executes.
2. Reverie reads the syscall number and six arguments from registers and stores
   them as the pending syscall.
3. Reverie decodes a typed `Syscall` and calls Detcore's
   `handle_syscall_event`.
4. Detcore runs its common pre-handler hook, advances logical progress, and
   dispatches to a syscall-family handler.
5. The handler may wait for a scheduler turn, read or write guest memory,
   inject a real syscall, delegate to record/replay, or emulate the result.
6. Detcore logs the result, runs its post-handler hook, and returns a value or
   errno.
7. If no injection consumed the pending syscall, Reverie skips the original
   syscall so Linux cannot execute it after the emulated result is chosen.
8. Reverie writes the result to the return register and resumes the tracee,
   delivering a pending signal if required.

### The three syscall outcomes

Detcore handlers use three fundamental outcomes:

**Emulate.** The handler modifies guest memory and returns a value without
calling `Guest::inject`. Reverie sees that the original call is still pending,
steps past it without executing its effect, and installs the emulated result.
Virtual time queries and precise futex handling use this pattern.

**Inject or transform.** The handler asks Reverie to execute a syscall. If it is
the same syscall with the same arguments, Reverie can resume the pending call
and stop at syscall exit. For a transformed call, Reverie executes through its
untraced trampoline, restores the guest register context, and returns the Linux
result to Detcore. Detcore may then normalize output memory or return values.

**Tail-inject.** A successful call such as `execve` or `exit` does not return to
the old guest context. `tail_inject` transfers control to that syscall and
cancels the outstanding handler future.

"Passthrough" is implemented as injection, not as ignoring the seccomp stop.
This distinction guarantees that a trapped syscall executes at most once.

### Other event paths

The same pre/post hooks wrap signal, CPUID, RDTSC, timer, thread-start, and
post-exec events. Signals may be deferred until the scheduler grants an
`InboundSignal` turn. CPUID leaves can be served from a deterministic table.
RDTSC is trapped and converted to logical time when time virtualization is
enabled. Timer events provide control even when a guest runs without making a
syscall.

## Deterministic scheduling

Sequentialized execution is cooperative at event boundaries and enforced by
RCB-based preemption between them. It does not mean that Linux has only one
guest task. The kernel still owns the tasks; Detcore controls which stopped task
is allowed to continue.

### The check-in protocol

A handler that needs ordering constructs a `Resources` request and calls
`resource_request`. With `sequentialize_threads` disabled, that function returns
immediately. With it enabled:

1. The thread sends `RequestResources` to `GlobalState`, piggybacking its local
   logical time.
2. The global receiver fills that thread's request IVar and waits on its
   response IVar.
3. The scheduler selects a TID from its deterministic run queue.
4. It waits until that TID has parked with a request.
5. It either blocks/skips the request or commits the turn.
6. On commit, it fills the response IVar, optionally including a timeslice.
7. The handler resumes and performs the ordered operation.

```text
 guest handler                GlobalState                  Scheduler
      |                           |                            |
      | RequestResources + time  |                            |
      |-------------------------->| request IVar := resources  |
      |                           |--------------------------->|
      |       parked on response IVar                         |
      |                           |   choose by priority/RR    |
      |                           |   test blocked condition   |
      |                           |   COMMIT turn              |
      |                           |<---------------------------|
      |<--------------------------| response := Go(timeslice)  |
      | ordered effect / injected syscall                     |
      | release or next check-in |                            |
```

The run queue orders lower numeric priority first and uses deterministic
round-robin order within a priority. Separate structures track runnable,
time-blocked, futex-blocked, and external-I/O-blocked threads. A thread present
in a blocked pool is absent from the run queue.

### Scheduler turn

The scheduler daemon's steady-state loop is:

```text
  +----------------------------------------------------------+
  | 1. Wait for quiescence; advance scheduler logical time   |
  | 2. Process timers, futex wakes, signals, and blocked I/O  |
  | 3. Tentatively choose the next runnable TID              |
  | 4. Await that TID's request and test whether it can run   |
  | 5. COMMIT: reply Go/Signaled and increment the turn       |
  | 6. Requeue, reprioritize, or leave the TID blocked        |
  | 7. Apply synthetic exit post-processing when needed      |
  +-----------------------------+----------------------------+
                                |
                                +---- next turn ---->
```

The current implementation deliberately checks for broad quiescence before
advancing a turn. This is conservative and limits parallelism, but makes the
global snapshot and commit order well defined.

Potentially blocking internal operations are either modeled precisely or
converted to nonblocking polling. Poll attempts are deterministically backed
off in the run queue, then periodically promoted to prevent starvation.
External blocking I/O is backgrounded and must check back in with a
`BlockedExternalContinue` request; strict reproducibility may additionally
require record/replay for its result.

### RCB preemption and busy loops

Syscall boundaries alone are not productive: a thread can spin forever without
entering the kernel. When configured, Reverie's performance-counter clock
counts retired conditional branches (RCBs). Detcore projects those counts into
logical time and arms a timer for the remaining timeslice.

Every handler pre-hook reads newly retired branches and checks whether the
timeslice ended. The post-hook checks again because the event itself can advance
logical time, then arms the next RCB timer. A timer stop invokes the same hooks,
ends the slice, and parks the thread on a yield or priority-change request.

This protocol is why all event handlers must preserve the common pre/post-hook
discipline.

### Thread lifecycle ordering

The parent registers a child with the scheduler during clone/fork handling. The
child's thread-start hook then blocks until the scheduler grants its first
turn. On exit, Detcore deregisters the thread, removes it from scheduler
structures, and contributes its final logical time. `exit_group` is represented
as a scheduler request so process-tree teardown has a deterministic commit
point.

## Resource model

`ResourceID` names guest-visible state whose effects need ordering. Current
identities include file contents and metadata by deterministic inode, directory
contents, paths, devices, a process memory address space, sleeps, exits, futex
waits, signals, and internal/external I/O protocol events.

A request maps identities to `R`, `W`, or `RW` permission and includes the TID,
poll attempt, and a diagnostic label. This is a scheduling model, not a claim
that Detcore implements every Linux object. Handlers usually acquire an order,
let Linux perform the real operation, and then determinize the observable
parts.

Current limitations matter when extending the model:

- the scheduler currently accepts at most one resource identity per turn;
- its one-resource blocking path does not yet use the `R`/`W` distinction to
  admit parallel readers;
- the memory resource is the whole `MemAddrSpace(DetPid)` and does not model
  shared-page aliasing between address spaces;
- several resource-table fields anticipate finer-grained concurrency but the
  current scheduler remains conservatively sequential.

### Files and file descriptors

`FileMetadata` is Detcore's model of a process file descriptor table. It maps a
raw FD to a `DetFd` containing its type, logical flags, path, deterministic
inode, cached raw stat data, resource identity, and whether Detcore forced the
underlying FD into nonblocking mode.

```text
 raw FD 3
    |
    v
 DetFd { type, flags, path, deterministic inode, resource, ... }
    |                                      |
    | Linux operation                      | scheduler identity
    v                                      v
 host kernel FD                     FileContents(inode)
                                    FileMetadata(inode)
                                    Path(...)
```

Threads created with `CLONE_FILES` share the `Arc<Mutex<FileMetadata>>`.
Otherwise the table is copied. Duplication and close handlers keep it aligned
with Linux. Successful exec removes close-on-exec entries; a failed exec
restores the prior table.

Raw inode numbers are mapped through the global inode pool to deterministic
inode numbers. Virtualized metadata uses the configured epoch and logical
mtime updates. File existence and content can still be external inputs unless
the operation is recorded or otherwise controlled by the environment.

### Futexes

In precise mode, Detcore emulates `FUTEX_WAIT` and `FUTEX_WAKE` instead of
executing them in the kernel. A futex is currently identified by
`(DetPid, virtual_address)`. A waiter is removed from the run queue and stored
in a scheduler wait set until a matching deterministic wake, timeout, or signal
makes it runnable. Wake returns the scheduler's deterministic wake count.

```text
 FUTEX_WAIT                         FUTEX_WAKE
 value check                           |
     |                                 v
 WaitRequest(pid, address) -----> scheduler wait set
     |                                 |
 thread leaves run queue          choose matching waiters
     |                                 |
     +<---------- Go / timeout / signal+
```

The identity does not yet support inter-process shared futex aliasing. Detcore
also has polling and external blocking modes for debugging or compatibility;
those have different determinism/performance tradeoffs.

### Memory

Memory is read and written through Reverie's `Guest` memory API while the
tracee is stopped. Memory-affecting operations can request
`MemAddrSpace(DetPid)`, which serializes at process-address-space granularity.
This prevents same-process memory operations from being reordered by the
scheduler, but is intentionally coarser than Linux VM objects and cannot express
all cross-process shared mappings.

### Internal and external blocking

For internal resources whose readiness is not modeled exactly, handlers make
the real FD nonblocking and retry under `InternalIOPolling` turns. Each retry is
part of the deterministic syscall history. For an endpoint outside the
container, the scheduler uses `BlockingExternalIO` and
`BlockedExternalContinue` to avoid deadlocking all guest threads while the host
event is pending. The exact completion time of such an external event is not
made deterministic by scheduling alone.

## Virtual time

Virtual time is derived from deterministic progress, not host wall-clock time.
Each thread owns a `DetTime` with separate counters:

```text
 local_ns = epoch_ns
          + multiplier * (10,000 ns * syscalls
                         +    10 ns * retired conditional branches
                         +    25 ns * CPUID/RDTSC events)
```

The constants are model parameters, not measurements of physical instruction
latency. When sequentialization is enabled without RCB time, Detcore applies an
additional multiplier to compensate for the sparse syscall clock.

`GlobalTime` maintains a vector of each thread's progress plus scheduler time:

```text
 global_ns = epoch_ns
           + sum(per-thread progress since epoch)
           + scheduler/external logical time
```

Every global RPC advances the sender's vector component monotonically.
Committed scheduler turns add a fixed scheduler-time quantum when appropriate.
Skipped and internal bookkeeping turns do not advance it; periods waiting only
for external events are treated separately.

Time-related syscalls such as `clock_gettime`, `gettimeofday`, and `time` ask
`GlobalState` for the aggregate lower bound and write that deterministic value
into guest memory. Logical sleeps become `SleepUntil` scheduler requests and
resume when committed global time reaches the target.

RDTSC currently returns the calling thread's local logical nanoseconds as
virtual cycles, rather than aggregate `GlobalTime`. This difference is
intentional in the current implementation and should be considered when
changing either clock path.

Virtual time also drives deterministic file timestamps, alarm deadlines,
sleep timeouts, and scheduler accounting. Code that waits on time must go
through the scheduler; reading a logical clock alone cannot make time advance.

## Chaos mode

Chaos mode explores different valid schedules while remaining reproducible for
a fixed configuration and seed. It requires sequentialized scheduling.

There are separate deterministic random streams for:

- application-facing random data, such as virtualized `getrandom`;
- per-thread chaos decisions, derived from the scheduler seed and ancestry;
- scheduler/run-queue choices;
- optional semantic fuzzing such as futex behavior.

Keeping these streams separate prevents an application random read from
silently changing the schedule.

```text
 scheduler seed
      |
      +--> run-queue PRNG --> random/sticky scheduling heuristic
      |
      +--> root chaos PRNG --derive by pedigree--> child chaos PRNGs
                                      |
                                      +--> starting priorities
                                      +--> exponential RCB timeslices
                                      +--> priority change points
```

Chaos timeslices are sampled in RCB units from an exponential distribution.
At a timeslice boundary, the thread checks in with a deterministic priority
change. The run queue may use round-robin, random, or sticky-random selection,
but the configured seed makes the sequence repeatable.

`--record-preemptions` captures per-thread slice endpoints and priorities.
Replay can feed those points back into `ThreadState::next_timeslice`; schedule
event tracing/replay provides a more detailed event order. These mechanisms
make a discovered schedule diagnosable without turning nondeterministic host
timing into part of the test case.

## Determinism boundaries

Hermit controls ordering only for events it intercepts and models. Contributors
should distinguish these cases:

| Case | Typical treatment |
| --- | --- |
| Purely virtual result | Emulate and suppress the Linux syscall |
| Real operation with unstable output fields | Inject, then rewrite outputs |
| Ordered internal side effect | Acquire a scheduler turn, then inject |
| Precisely modeled blocking primitive | Park in a Detcore wait structure |
| Imprecisely modeled internal blocking | Nonblocking deterministic polling |
| External input | Record/replay or accept an explicit determinism boundary |
| Unsupported subscribed syscall | Panic when configured, otherwise inject passthrough |

Namespaces, CPU affinity, filesystem setup, and environment normalization
reduce inputs before tracing starts. They complement Detcore; they do not
replace syscall-level policy. Likewise, ptrace interception alone does not make
an event deterministic. The handler must define its observable result and its
place in the schedule.

## Adding or changing a handler

When extending Detcore, check the complete path:

1. Add the syscall to the optimized-build subscription.
2. Decode it in `Detcore::handle_syscall_event` and preserve common hooks.
3. Decide whether the result is emulated, injected, transformed, or delegated
   to record/replay.
4. Identify guest memory inputs and outputs and access them only while stopped.
5. Define the scheduler/resource request before any guest-visible side effect.
6. Make blocking behavior precise, deterministically polled, or explicitly
   external.
7. Update FD, inode, time, or lifecycle models after successful Linux effects
   and roll them back on failure where necessary.
8. Test errors and signals as well as the successful path.
9. Test optimized subscriptions; debug builds tracing everything can hide a
   missing subscription.

The syscall must execute exactly once. Returning an emulated value without
injecting suppresses it; injecting transfers execution responsibility to
Reverie. Do not inject an effect and then accidentally leave a second path that
can execute the original call.

## Source map

The main implementation entry points are:

| Area | Source |
| --- | --- |
| CLI and runtime construction | `hermit-cli/src/bin/hermit/run.rs`, `hermit-cli/src/lib.rs` |
| Container setup | `hermit-cli/src/bin/hermit/container.rs` |
| Detcore `Tool` implementation and dispatch | `detcore/src/lib.rs` |
| Per-thread/process state | `detcore/src/tool_local.rs` |
| Global RPC state | `detcore/src/tool_global.rs` |
| Scheduler daemon and turn protocol | `detcore/src/scheduler.rs` |
| Runnable selection and polling backoff | `detcore/src/scheduler/runqueue.rs` |
| Resource identities and requests | `detcore/src/resources.rs` |
| FD model | `detcore/src/fd.rs` |
| Logical time | `detcore-model/src/time.rs`, `detcore/src/syscalls/time.rs` |
| Syscall-family handlers | `detcore/src/syscalls/` |
| Reverie tool contract | Reverie `reverie/src/tool.rs` and `reverie/src/guest.rs` |
| Ptrace startup/filter | Reverie `reverie-ptrace/src/tracer.rs` |
| Ptrace stop and injection mechanics | Reverie `reverie-ptrace/src/task.rs` |

Start at the Detcore dispatch for policy questions and at Reverie's tracing task
for execution-control questions. Scheduler bugs often cross both local and
global Detcore state, so trace the request IVar, response IVar, run-queue entry,
and logical-time update as one protocol.
