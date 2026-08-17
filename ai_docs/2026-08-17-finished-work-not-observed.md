# Finished work that surrounding machinery did not observe

Date: 2026-08-17

This document records four independently measured cases with the same visible
shape: something finishes, but the machinery around it does not observe that
completion correctly.

This is **not** a claim of one root cause. The four cases live in different
components and have different mechanisms and resolution states. The common
shape is useful for review and test design; it is not evidence that one code
change can fix all four.

| Component | What finished | What was not observed | State |
|---|---|---|---|
| KVM, `signal-waitstatus-identity` | the guest logically exited after reporting wait-status failures | KVM did not complete physical teardown | open KVM product defect; ptrace completes |
| KVM, `pipe-chain` | the parent logically exited after a leaked `ERESTARTSYS` | KVM did not complete physical teardown while stage processes remained | open KVM product defect; ptrace passes |
| DBT evidence | the scheduler process returned from `runtime_background_init` and called `exit(0)` | the required final evidence frame was never constructed | root-caused; blocked on ownership |
| SaBRe shutdown evidence | the final exit-barrier release completed | two scheduler empty-state checks did not observe it in a stable order | fixed in #2304 |

## 1. KVM teardown: `backend-parity-c/signal-waitstatus-identity`

### Guest and backend context

The guest forks children that exit normally, die from signals, or exit from a
non-main thread. The parent decodes the result returned by `wait4` and asserts
that normal exits and signal deaths retain their Linux wait-status identity.

Measured with source commit
`39a01d4b13a98e3eff8d910c5609557cc647ac4c` and release binary SHA-256
`00c5412fa6c23360c5fc49c9d6b6789b585f2937ac7a62b9c6a442176e106cde`.

Canonical KVM command:

```bash
env HERMIT_BIN="$PWD/target/release/hermit" \
  E2E_RESULT_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/hang-characterization/signal-waitstatus-kvm/evidence" \
  E2E_BUILD_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/batch-01/build" \
  E2E_RUN_ID=signal-waitstatus-kvm-retained \
  E2E_KEEP_VERIFY_LOGS=1 \
  target/debug/test-harness run \
    --test backend-parity-c/signal-waitstatus-identity \
    --mode verify --backend kvm --probe-disabled
```

This was blocked rather than CPU-bound: 3.14 user seconds plus 0.28 system
seconds over 123.39 wall seconds, or 2% aggregate CPU.

The real guest output before teardown stopped was:

```text
harness: FAIL fork/waitpid did not complete
normal_exit: UNEXPECTED neither-exited-nor-signalled
normal_exit: FAIL expected exit code=7
sigterm: exited code=143
sigterm: FAIL expected death by signal 15, got a normal exit
sigkill: exited code=137
sigkill: FAIL expected death by signal 9, got a normal exit
sigill: exited code=132
sigill: FAIL expected death by signal 4, got a normal exit
sigfpe: exited code=136
sigfpe: FAIL expected death by signal 8, got a normal exit
abort: exited code=134
abort: FAIL expected death by signal 6, got a normal exit
harness: FAIL fork/waitpid did not complete
exit_from_non_main_thread: UNEXPECTED neither-exited-nor-signalled
exit_from_non_main_thread: FAIL expected exit code=11
failures=11
```

In the final threaded-child case, the child thread calls `exit_group(11)`. The
parent's `wait4(10, ...)` returns `ERESTARTSYS` to the guest rather than
completing with the child's status. The root guest reports its failures and
calls `exit_group(1)`.

The last KVM scheduler record is COMMIT turn 175737, on previously committed
virtual time `1_767_225_643.969_376_250s`
(`1767225643969376250` virtual nanoseconds). The pending guest syscall is #60,
`exit_group(1)`. The run never produces a total-turn report because physical
teardown does not complete.

The ptrace control proves that the teardown failure is backend-specific. One
canonical control completed in 5.68 seconds and matched 332/332 INFO messages.
A retained second control also completed, but exposed the separate ptrace
child-reap divergence at turn 185 of a 205-turn reference run. Ptrace therefore
does not share the KVM teardown failure even though this guest can also expose
the already-known ptrace `wait4` nondeterminism.

**State:** open KVM product defect.

## 2. KVM teardown: `determinism-stress-c/pipe-chain`

### Guest and backend context

The guest creates five child stages connected through six pipes. Each stage
reads its input, appends a fixed line, forwards the result, and exits with a
fixed status from 30 through 34. The parent reads the final pipe, waits for all
five statuses, verifies the byte stream, and prints it.

Canonical KVM command:

```bash
env HERMIT_BIN="$PWD/target/release/hermit" \
  E2E_RESULT_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/hang-characterization/pipe-chain-kvm/evidence" \
  E2E_BUILD_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/batch-01/build" \
  E2E_RUN_ID=pipe-chain-kvm-retained \
  E2E_KEEP_VERIFY_LOGS=1 \
  target/debug/test-harness run \
    --test determinism-stress-c/pipe-chain \
    --mode verify --backend kvm --probe-disabled
```

This was also blocked rather than CPU-bound: 3.16 user seconds plus 0.28
system seconds over 93.42 wall seconds, or 3% aggregate CPU.

The relevant INFO records are:

```text
INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: read(13, 0x3fffc8e0, 4096) = ?
INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #56: read(13, 0x3fffc8e0, 4096) = Err(Errno(ERESTARTSYS))
```

That internal restart result leaks to the guest as errno 512 instead of the
read being restarted. The guest prints:

```text
read(output): Unknown error 512
```

It then calls syscall #62, `exit_group(1)`. The last scheduler record is COMMIT
turn 39, on previously committed virtual time
`1_767_225_600.028_259_250s` (`1767225600028259250` virtual nanoseconds).
Three stage processes are still blocked in the pipe protocol when the parent
exits. The parent finishes logically, but KVM never completes physical
teardown.

The ptrace control completes in 3.52 seconds, runs 67 scheduler turns, and
matches 471/471 INFO messages canonically.

**State:** open KVM product defect.

## 3. DBT evidence: scheduler process exits without a final frame

### Process and evidence context

The DBT runtime starts its scheduler through `dr_create_client_thread`. That
thread is owned by a distinct scheduler process/PID rather than by the guest
process that emits the other protocol frames. Its START and DATA frames are
therefore emitted under its own `SO_PEERCRED` identity.

After `runtime_background_init` returns, that process calls `exit(0)` without a
corresponding `thread_leave` or `event_exit` callback. The producer that would
construct its FINAL frame is never called. The process has finished, but the
evidence protocol does not record its completion.

The concrete `c-programs/ioctl-fioclex` measurement showed sequence 11 rather
than 10: the scheduler process contributed one additional DATA frame for its
`/dev/null` read, but never contributed its FINAL frame. Six DBT cells ran
twice, exited zero, and matched their backend-local memory hashes while the
canonical report remained unwritten; this missing FINAL frame is one of the
ownership blockers behind that evidence gap.

The current Rust entry point is
`detcore-dbt/src/lib.rs::reverie_dbt_runtime_background_init`; it runs the
external scheduler and returns after emitting `background scheduler
completed`. The missing lifecycle callback is in the distinct native DBT
client/process ownership path, not in the KVM code described above.

Runnable cell selector:

```bash
target/debug/test-harness run \
  --test c-programs/ioctl-fioclex \
  --mode verify --backend dbt --include-manual
```

**State:** root-caused and blocked on ownership. No fix is claimed by this
document.

## 4. SaBRe shutdown evidence: final release raced empty-state checks

### Scheduler and evidence context

SaBRe's final physical-exit-barrier release raced two scheduler empty-state
checks. Byte-identical guest execution could therefore produce one run with
the existing

```text
scheduler (step2_process_blocked): zero threads left anywhere, fizzling.
```

diagnostic and another run without it. Five measured cells showed this
evidence-order difference. The fix did not relax or normalize the comparator;
it made the observation order deterministic.

PR #2304 / commit `cff8ea3c9d3aa04eccfe50b80421a19acafd4064`
moved the diagnostic to the scheduler's single terminal exit and published the
SaBRe backend fact only after scheduler cleanup. Its commit evidence names two
concrete examples:

- `c-programs/io-uring-ring-determinism`: 124/123 INFO before the fix.
- `c-programs/periodic-setitimer-delivery`: 148/149 INFO before the fix.

The regression test requires exactly one scheduler-fizzle line, exactly one
scheduler-empty line, exactly one backend-evidence fact, and this order:

```text
scheduler fizzle < scheduler empty < backend evidence < fallback completed
```

Runnable selectors for the named cells are:

```bash
target/debug/test-harness run \
  --test c-programs/io-uring-ring-determinism \
  --mode verify --backend sabre --include-manual

target/debug/test-harness run \
  --test c-programs/periodic-setitimer-delivery \
  --mode verify --backend sabre --include-manual
```

**State:** fixed in #2304 with canonical regression coverage.

## What is shared, and what is not

Shared observable shape:

1. A guest, process, scheduler, or exit barrier reaches its completion point.
2. An adjacent layer fails to observe, publish, or order that completion.
3. The outer result is a hang, missing evidence, or differing evidence rather
   than an accurate account of what already happened.

Different mechanisms:

- The two KVM cases include guest-visible syscall/status defects and incomplete
  backend teardown.
- The DBT case is lifecycle callback and evidence-frame ownership across a
  distinct scheduler PID.
- The SaBRe case was an observation-order race between a physical-exit barrier
  and scheduler empty-state checks.

These distinctions are load-bearing. This document does not assert a common
root cause or propose a common fix.
