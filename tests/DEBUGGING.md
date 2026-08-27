# applications

# backend-parity-c

## backend-parity-c/hardware-trap-identity

### 2026-08-27T05:27:07-07:00 — portable / verify / dbt — ERROR

- Hermit: `6172478eae288c9f5005545d4f68c00c780f41c0`; complete published E2E
  artifact binary SHA-256:
  `82e5d9e0f1ba554a2cedf4e970e94c43761bbca42087a2797443aca22c0da695`.
  Later main commit `f857a94ef7f943cacb2827177d876a296cfde989`
  moved the Reverie pin onto main without changing the DBT runtime source.
- Command: each retained result row contains the runner-generated `hermit --log
  info run --base-env=minimal --backend dbt --strict --verify-strict --verify
  --keep-logs` invocation. The checked-in timeout remained 15 seconds, one
  scheduled worker, and no comparison relaxation.
- Evidence: three result rows and their artifacts are retained under
  `<dev-hermit>/ignored/hermit-132-postfix-6172478e-retry4-results/`. The two
  complete Run1 logs have SHA-256
  `03a22d52750074e8d661e173540a1495f288272c3d4fa67b68e57a78adbc51bb`.
  The pre-fix record contents and artifact hashes remain preserved in
  [`tests/backend-parity/README.md`](backend-parity/README.md#hardware-trap-identity-pre-fix-dbt-child-exit-polling-changed-the-canonical-log).
- Observed: 3 of 3 attempts returned `ERROR` with `verdict=no_result`; no
  terminal comparison exists. Two attempts completed an identical Run1 trace
  with no `InternalIOPolling` and ended after the root entered `exit_group(0)`,
  then timed out after starting Run2. The other attempt timed out in Run1 with
  an empty log.
- Current explanation: before Hermit `221c3d77959ebdef08f2890aaf3ce5185ea5d425`,
  DBT alone set `backend_tracks_process_children` false. When the parent reached
  blocking `wait4` before the child exit was published, Detcore used the generic
  nonblocking retry path and recorded a host-dependent number of
  scheduler-visible polling turns. The landed repair removes that record
  pattern; it does not complete the exact cell. The guest's #UD, #DE, and #PF
  signals and `si_code` values were correct in the pre-fix direct runs, so the
  cell name does not identify the failing subsystem.
- Ruled out / next: this is not the SaBRe TLS ordering defect and not KVM's
  wrong guest-visible signal result. The pre-fix polling mechanism was shared
  with `backend-parity-c/signal-waitstatus-identity/verify@dbt`, which is also
  now 3 of 3 `ERROR/no_result`. Determine why DBT does not complete both runs
  after the child-lifecycle repair; do not restore host polling and do not call
  either cell resolved until a terminal canonical comparison exists.

## backend-parity-c/signal-waitstatus-identity

### 2026-08-27T05:27:07-07:00 — portable / verify / dbt — ERROR

- Hermit and command: the same main SHA, complete E2E artifact, unrelaxed
  comparator, 15-second timeout, and one-worker runner as
  `backend-parity-c/hardware-trap-identity` above.
- Evidence: three result rows and artifact directories under
  `<dev-hermit>/ignored/hermit-132-postfix-6172478e-retry4-results/`, with
  result-row SHA-256 values
  `585956b5b9a61009b0abac79befa75db622c1bc3df83c6937d78db27ddbefbfd`,
  `abe3254fd3b79e28318463892b7d57c963d453e729386d5ca06287945d952d19`,
  and `ef2bdddcf6039c044c0a43dcacde54d53728dc46d908b4af7b7ffe51edb83e48`.
- Observed: 3 of 3 attempts returned `ERROR` with `verdict=no_result`; each
  stopped in DBT Run1 and retained an empty canonical log. No coordinate or
  record contents exist to import.
- Current explanation: the old 3-of-3 divergence shared hardware-trap's
  `wait4`/`InternalIOPolling` mechanism while guest output ended in
  `failures=0`. The landed child-lifecycle repair removed that host-polling
  path, but this exact signal-terminated-child scenario still does not complete.
- Ruled out / next: ptrace was the positive control for the old comparison at
  336/336 canonical INFO records. Re-measure DBT only after its run completes;
  until then this cell is uncheckable rather than matching or diverging.

# bin-c

# c-programs

# chaos-c

# data-handling

# debugger-c

# determinism-stress

# determinism-stress-c

# language-runtimes

# shared-futex-c

# system-utils

# util-c
