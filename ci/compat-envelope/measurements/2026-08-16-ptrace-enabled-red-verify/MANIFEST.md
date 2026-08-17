# Enabled-red ptrace verify measurement

- Hermit commit: `79cc84087904bf7069cef9c585a2eac041341d40`
- Branch used: `codex/ptrace-red-cell-first-divergence-survey`
- Date: 2026-08-16 America/Los_Angeles (2026-08-17 UTC)
- Population: complete run of all 186 cells in `ci/compat-envelope/cells.json`
  with `backend=ptrace`, `mode=verify`, `enabled=true`, and `status=red`
- Result: 176 passed canonically, 1 diverged, and 9 could not produce a
  comparable two-run result
- Backend: ptrace (the default backend; no backend override was passed)
- Relaxations: none

Each guest was run with this canonical invocation shape, plus its manifest
guest arguments where declared:

```text
target/release/hermit --log info run --strict --verify \
  --verify-json verify.json --keep-logs --verify-log-dir logs -- \
  PROGRAM [ARGS...]
```

`--verify-json` and the log-retention flags preserve evidence; they do not
weaken comparison. A pass required `verdict="matched"`,
`bitwise_parity=true`, and nonempty canonical INFO comparison. C guests were
compiled from the checked-in source and manifest C flags. The
`record-replay-lseek-seek-cur` guest was rerun from the repository root because
its declared `README.md` argument is relative; the corrected run passed with
223/223 INFO messages.

The one canonical divergence was
`backend-parity-c/signal-waitstatus-identity`: after both runs returned
`ERESTARTSYS` from `wait4`, run 1 logged a wait4 `InternalIOPolling` NONCOMMIT
while run 2 logged `wait4(...)=Ok(17)` at the same INFO position. The typed
report records first divergent scheduler turn 53 and 422/342 INFO messages.

The nine cells without a comparable two-run result were:

- `bin-c/robust-futex-test`: first run deadlocked with two futex waiters and no
  runnable threads.
- `c-programs/dbt-pid-virtualization`: verification timed out after both runs
  started.
- `c-programs/nanosleep-threads-simple`: first run terminated with SIGSEGV.
- `c-programs/resource-determinism`: first-run assertion failed because the
  elapsed clock did not advance across logical work.
- `shared-futex-c/qemu-exec-init`: first run did not complete after
  `execve("/hello")` returned `ENOENT`.
- `shared-futex-c/qemu-hello`: intentionally exits 7, while default verify
  requires a successful first run.
- `shared-futex-c/qemu-init`: first run waited indefinitely with no runnable
  threads.
- `shared-futex-c/qemu-net-init`: first run terminated with SIGSEGV after its
  network probes.
- `util-c/pmu-skid`: nested ptrace setup failed at
  `PTRACE_TRACEME`/`PTRACE_KILL`.

`summary.json` contains the complete typed aggregate and per-cell records.
`results.jsonl` contains one durable record per cell. No binaries, build output,
or temporary logs are included.

## Harness-invocation spot checks

The complete 186-cell run above used the owner's canonical invocation shape.
The portable E2E harness additionally sets its isolated environment, explicitly
selects ptrace, and passes `--no-virtualize-cpuid
--max-timeslice=disabled`. To test whether those differences invalidate the
canonical-pass observations, five IDs were sampled without replacement from
the sorted 176-cell pass list using random seed `12413261074948613501`:

- `c-programs/unix-autobind-stream`
- `c-programs/meminfo-available-deterministic`
- `c-programs/name-to-handle-at-eopnotsupp`
- `c-programs/sigpipe-siginfo`
- `debugger-c/debuggee`

Each cell ran five times with the owner's shape and five times with the actual
harness shape. All 50 runs passed canonical INFO comparison. The owner and
harness profiles had identical INFO counts within every cell and attempt; there
were no divergences or incomplete runs.

All five scorecard cells have `observations=[]`, are absent from
`ci/expected-e2e-plan.json`, and have manifest verify `ci=false`. Therefore no
checked-in result row exists for any of the five; their red status is not a
recorded runtime failure.

`SPOT_CHECKS.md` preserves every literal command, literal output, and per-run
typed verdict. `spot-check-summary.json` and `spot-check-results.jsonl` preserve
the structured 50-run aggregate.
