# Finished work that surrounding machinery did not observe

Date of measurements: 2026-08-17

Status checked: 2026-08-27 at Hermit
`ffd409b28ec81a97b6f713b11823e57169bc3220`, which pins Reverie
`ab07a89239150df3726a036bee9f5e897893dfc1`.

This document records four independently measured cases with the same visible
shape: something finishes, but the machinery around it does not observe that
completion correctly.

This is **not** a claim of one root cause. The four cases live in different
components and have different mechanisms and resolution states. The common
shape is useful for review and test design; it is not evidence that one code
change can fix all four. The measurements below remain tied to their stated
source revisions. The current-status notes distinguish those measurements from
what later landed.

| Component | What finished | What was not observed | Current status |
|---|---|---|---|
| KVM, `signal-waitstatus-identity` | the guest logically exited after reporting wait-status failures | KVM did not complete physical teardown | the two diagnosed mechanisms have landed fixes; the exact cell has not been remeasured at the current pin |
| KVM, `pipe-chain` | the parent logically exited after a leaked `ERESTARTSYS` | KVM did not complete physical teardown while stage processes remained | the leaked `ERESTARTSYS` was fixed and the original cell passed 5/5 with that fix; the exact cell has not been remeasured at the current pin |
| DBT evidence | the scheduler process returned from `runtime_background_init` and called `exit(0)` | the required final evidence frame was never constructed | this missing-final-frame mechanism was fixed; later DBT completion failures are a different condition |
| SaBRe shutdown evidence | the final exit-barrier release completed | two scheduler empty-state checks did not observe it in a stable order | the backend-evidence ordering fix landed; the proposed exactly-once scheduler message did not land because it would hide measured guest-visible nondeterminism |

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

The ptrace control completed in 5.68 seconds and matched 332/332 INFO messages.
A retained second control also completed, but exposed a separate ptrace
child-reap divergence at turn 185 of a 205-turn reference run. This establishes
that the observed KVM teardown failure was not a failure of the guest alone; it
does not make ptrace a universal reference for every fork/exec path.

### Later disposition

The Reverie revision now pinned by Hermit carries `ExitStatus` through the KVM
child process path rather than flattening fatal signals into `128 + signal` and
contains the KVM consumer for Tool-returned `ERESTARTSYS`. The relevant landed
commits are
[`a16e3c466a15c3746a5ef23a76d1f74e11aba935`](https://github.com/rrnewton/reverie/commit/a16e3c466a15c3746a5ef23a76d1f74e11aba935)
and
[`a11fedcb168f7f8fa15681dacc4824573716c2f4`](https://github.com/rrnewton/reverie/commit/a11fedcb168f7f8fa15681dacc4824573716c2f4).
Those changes address the two mechanisms measured above. They do not establish
the exact current cell result: the current manifest records KVM as not
applicable on the current host because KVM cannot complete a guest there, and no
current-pin exact-cell remeasurement is cited here.

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

That internal restart result leaked to the guest as errno 512 instead of the
read being restarted. The guest printed:

```text
read(output): Unknown error 512
```

It then called syscall #62, `exit_group(1)`. The last scheduler record was
COMMIT turn 39, on previously committed virtual time
`1_767_225_600.028_259_250s` (`1767225600028259250` virtual nanoseconds).
Three stage processes were still blocked in the pipe protocol when the parent
exited. The parent finished logically, but KVM never completed physical
teardown.

The ptrace control completed in 3.52 seconds, ran 67 scheduler turns, and
matched 471/471 INFO messages canonically.

### Later disposition

Reverie commit
[`a11fedcb168f7f8fa15681dacc4824573716c2f4`](https://github.com/rrnewton/reverie/commit/a11fedcb168f7f8fa15681dacc4824573716c2f4)
added the missing KVM consumer for `ERESTARTSYS`. Its retained A/B evidence on
this guest changed the result from a 90-second timeout with one visible errno
512 to exit 0 with stdout matching the ptrace control, and the fixed arm passed
5 of 5 runs. That establishes the original mechanism and its correction at the
stated source revisions. The exact cell has not been requalified at the current
Hermit/Reverie pair, so this document does not promote it.

## 3. DBT evidence: scheduler process exits without a final frame

### Process and evidence context

The DBT runtime starts its scheduler through `dr_create_client_thread`. That
thread is owned by a distinct scheduler process/PID rather than by the guest
process that emits the other protocol frames. Its START and DATA frames are
therefore emitted under its own `SO_PEERCRED` identity.

After `runtime_background_init` returned, that process called `exit(0)` without
a corresponding `thread_leave` or `event_exit` callback. The producer that
would construct its FINAL frame was never called. The process had finished,
but the evidence protocol did not record its completion.

The concrete `c-programs/ioctl-fioclex` measurement showed sequence 11 rather
than 10: the scheduler process contributed one additional DATA frame for its
`/dev/null` read, but never contributed its FINAL frame. Six DBT cells ran
twice, exited zero, and matched their backend-local memory hashes while the
canonical report remained unwritten; this missing FINAL frame was one of the
ownership blockers behind that evidence gap.

The Rust entry point remains
`detcore-dbt/src/lib.rs::reverie_dbt_runtime_background_init`; it runs the
external scheduler and returns after emitting `background scheduler
completed`. The missing lifecycle callback was in the distinct native DBT
client/process ownership path, not in the KVM code described above.

Runnable cell selector:

```bash
target/debug/test-harness run \
  --test c-programs/ioctl-fioclex \
  --mode verify --backend dbt --include-manual
```

### Later disposition

Reverie commit
[`bfbe3b14d5d4095c8d23bdb0c4ea278beca7b9c7`](https://github.com/rrnewton/reverie/commit/bfbe3b14d5d4095c8d23bdb0c4ea278beca7b9c7)
flushes the scheduler sender's FINAL after the background callback returns and
before publishing background quiescence. Its tests kept the collector
fail-closed for every admitted process image without FINAL.

This fixes the missing-final-frame mechanism recorded here. It does not make
all DBT verification complete. Current
[`tests/DEBUGGING.md`](../tests/DEBUGGING.md) records that
`backend-parity-c/signal-waitstatus-identity/verify@dbt` later produced three
`ERROR/no_result` attempts with empty Run1 canonical logs after a separate
child-lifecycle repair. That later condition has no terminal comparison and is
not evidence that this FINAL fix failed.

## 4. SaBRe shutdown evidence: final release raced empty-state checks

### Scheduler and evidence context

SaBRe's final physical-exit-barrier release raced two scheduler empty-state
checks. Byte-identical guest execution could therefore produce one run with
the existing

```text
scheduler (step2_process_blocked): zero threads left anywhere, fizzling.
```

diagnostic and another run without it. Five measured cells showed this
evidence-order difference.

The original candidate in
[pull request 2304](https://github.com/rrnewton/hermit/pull/2304) moved the
diagnostic to the scheduler's single terminal exit and published the SaBRe
backend fact only after scheduler cleanup. Its commit evidence named two
concrete examples:

- `c-programs/io-uring-ring-determinism`: 124/123 INFO before the candidate.
- `c-programs/periodic-setitimer-delivery`: 148/149 INFO before the candidate.

The candidate regression test required exactly one scheduler-fizzle line,
exactly one scheduler-empty line, exactly one backend-evidence fact, and this
order:

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

### Later disposition

The original document called this fixed in pull request 2304. That was wrong:
the pull request closed unmerged. Later review established that the number of
times the empty-queue branch is reached carries evidence of a guest-visible
SaBRe `wait4(WNOHANG)` difference. Current `detcore/src/scheduler.rs` records
the measured result: 36 of 50 strict-verification attempts diverged under
SaBRe, compared with 0 of 50 under ptrace, with 355 versus 354 guest syscalls
and four versus three `wait4` calls. Moving the message to a single terminal
emission would make its count constant without fixing that difference.

The independent ordering improvement did land as Hermit commit
[`5465e1451c3783a37cb114e5e608d5dc79142345`](https://github.com/rrnewton/hermit/commit/5465e1451c3783a37cb114e5e608d5dc79142345):
the SaBRe backend fact is published after awaited scheduler cleanup. The
per-occurrence scheduler diagnostic remains at INFO by owner ruling. The
current work is to make the child's exit observable to its parent at a
deterministic point, not to make the evidence stop reporting the difference.

## What is shared, and what is not

Shared observable shape:

1. A guest, process, scheduler, or exit barrier reaches its completion point.
2. An adjacent layer fails to observe, publish, or order that completion.
3. The outer result is a hang, missing evidence, or differing evidence rather
   than an accurate account of what already happened.

Different mechanisms:

- The two KVM cases included guest-visible syscall/status defects and incomplete
  backend teardown.
- The DBT case was lifecycle callback and evidence-frame ownership across a
  distinct scheduler PID.
- The SaBRe case included an observation-order race, but later evidence showed
  that forcing one message count would hide a guest-visible child-wait
  difference rather than fix it.

These distinctions are load-bearing. This document does not assert a common
root cause or propose a common fix. It also does not cover task ownership,
review-retirement, duplicate task rows, run ownership, or coordinator holds;
those are separate cases even when they produce the same visible result of
finished work not being observed.
