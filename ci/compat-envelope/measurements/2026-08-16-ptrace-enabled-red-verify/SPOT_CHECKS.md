# Randomized enabled-red ptrace verify spot checks

- Commit: `79cc84087904bf7069cef9c585a2eac041341d40`
- Random seed: `12413261074948613501`
- Selection: five IDs sampled without replacement from the sorted 176-cell canonical-pass list
- Repetitions: five owner-shape and five harness-shape runs per cell
- Result: 50/50 passed canonically; no divergence and no incomplete run
- Scorecard state for every selected cell: `enabled=true`, `status=red`, `observations=[]`, absent from `ci/expected-e2e-plan.json`, and manifest verify `ci=false`

## `c-programs/unix-autobind-stream`

### owner attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-1/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-1/logs/run1_log_EgV7j
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-1/logs/run2_log_9GLBS
:: Success: deterministic. Determinism verified.
stream=08000
```

### owner attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-2/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-2/logs/run1_log_IOYGX
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-2/logs/run2_log_epaXH
:: Success: deterministic. Determinism verified.
stream=08000
```

### owner attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-3/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-3/logs/run1_log_yyB2p
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-3/logs/run2_log_xC8aR
:: Success: deterministic. Determinism verified.
stream=08000
```

### owner attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-4/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-4/logs/run1_log_oIulW
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-4/logs/run2_log_plSkG
:: Success: deterministic. Determinism verified.
stream=08000
```

### owner attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-5/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-5/logs/run1_log_8Ezd0
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/owner/attempt-5/logs/run2_log_Uv1Bs
:: Success: deterministic. Determinism verified.
stream=08000
```

### harness attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/logs/run1_log_yiUfd
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-1/logs/run2_log_hShA2
:: Success: deterministic. Determinism verified.
stream=08000
```

### harness attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/logs/run1_log_X4zHM
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-2/logs/run2_log_gILv7
:: Success: deterministic. Determinism verified.
stream=08000
```

### harness attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/logs/run1_log_u8lc2
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-3/logs/run2_log_UPiFq
:: Success: deterministic. Determinism verified.
stream=08000
```

### harness attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/logs/run1_log_pHlmb
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-4/logs/run2_log_pGxUH
:: Success: deterministic. Determinism verified.
stream=08000
```

### harness attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `116/116`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 116 | 116 messages total
Logs contain 114 | 114 detcore-specific messages
Logs contain 116 | 116 INFO messages
Logs contain 92 | 92 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (116 | 116 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/logs/run1_log_2I1MC
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_unix-autobind-stream/harness/attempt-5/logs/run2_log_eQBOa
:: Success: deterministic. Determinism verified.
stream=08000
```

## `c-programs/meminfo-available-deterministic`

### owner attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-1/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-1/logs/run1_log_gmBV8
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-1/logs/run2_log_QlikG
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### owner attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-2/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-2/logs/run1_log_sdwv3
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-2/logs/run2_log_Cd8TW
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### owner attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-3/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-3/logs/run1_log_ku2Dv
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-3/logs/run2_log_R5a7z
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### owner attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-4/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-4/logs/run1_log_EwRDI
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-4/logs/run2_log_Rt4Br
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### owner attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-5/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-5/logs/run1_log_T4ft8
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/owner/attempt-5/logs/run2_log_8YNv5
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### harness attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/logs/run1_log_yvUO1
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-1/logs/run2_log_K4EBl
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### harness attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/logs/run1_log_t7quV
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-2/logs/run2_log_C0fxs
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### harness attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/logs/run1_log_FXrbm
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-3/logs/run2_log_C11rH
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### harness attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/logs/run1_log_huYFv
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-4/logs/run2_log_7pmsI
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

### harness attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `135/135`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 135 | 135 messages total
Logs contain 133 | 133 detcore-specific messages
Logs contain 135 | 135 INFO messages
Logs contain 110 | 110 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (135 | 135 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/logs/run1_log_KTa6u
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_meminfo-available-deterministic/harness/attempt-5/logs/run2_log_o3itL
:: Success: deterministic. Determinism verified.
MemAvailable is deterministic
```

## `c-programs/name-to-handle-at-eopnotsupp`

### owner attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-1/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-1/logs/run1_log_KpOwU
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-1/logs/run2_log_uctVH
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### owner attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-2/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-2/logs/run1_log_GZ9Rw
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-2/logs/run2_log_I4dJQ
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### owner attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-3/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-3/logs/run1_log_LXclL
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-3/logs/run2_log_jBpbx
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### owner attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-4/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-4/logs/run1_log_Phviw
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-4/logs/run2_log_OQqcI
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### owner attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-5/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-5/logs/run1_log_YQfyD
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/owner/attempt-5/logs/run2_log_yQ1yk
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### harness attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/logs/run1_log_bBD9t
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-1/logs/run2_log_JYiXS
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### harness attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/logs/run1_log_2Rdkg
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-2/logs/run2_log_nt5pB
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### harness attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/logs/run1_log_0WiOI
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-3/logs/run2_log_IAsOh
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### harness attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/logs/run1_log_bXJlV
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-4/logs/run2_log_OzkbR
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

### harness attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/logs/run1_log_fIcDJ
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_name-to-handle-at-eopnotsupp/harness/attempt-5/logs/run2_log_qWduV
:: Success: deterministic. Determinism verified.
name_to_handle_at deterministically refused
```

## `c-programs/sigpipe-siginfo`

### owner attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-1/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-1/logs/run1_log_mQBwU
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-1/logs/run2_log_Fu909
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### owner attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-2/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-2/logs/run1_log_h4XfC
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-2/logs/run2_log_G5EmA
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### owner attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-3/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-3/logs/run1_log_nNCBa
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-3/logs/run2_log_0HSlz
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### owner attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-4/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-4/logs/run1_log_J7qMp
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-4/logs/run2_log_Lb6TV
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### owner attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-5/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-5/logs/run1_log_qMV6o
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/owner/attempt-5/logs/run2_log_rbmS2
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### harness attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/logs/run1_log_OT8CI
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-1/logs/run2_log_I0BPq
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### harness attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/logs/run1_log_SwwMm
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-2/logs/run2_log_vwmVr
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### harness attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/logs/run1_log_VmCvU
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-3/logs/run2_log_mjE7b
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### harness attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/logs/run1_log_7Sd22
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-4/logs/run2_log_6dyff
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

### harness attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `193/193`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 193 | 193 messages total
Logs contain 191 | 191 detcore-specific messages
Logs contain 193 | 193 INFO messages
Logs contain 157 | 157 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (193 | 193 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/logs/run1_log_jFBld
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/c-programs_sigpipe-siginfo/harness/attempt-5/logs/run2_log_m79gI
:: Success: deterministic. Determinism verified.
sigpipe-si-code=0
```

## `debugger-c/debuggee`

### owner attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-1/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-1/logs/run1_log_tECYw
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-1/logs/run2_log_HZdgd
:: Success: deterministic. Determinism verified.
pid=3 result=55
```
### owner attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-2/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-2/logs/run1_log_VHYf4
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-2/logs/run2_log_QYnby
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### owner attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-3/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-3/logs/run1_log_Bw8Zc
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-3/logs/run2_log_NeSRz
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### owner attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-4/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-4/logs/run1_log_FkTKg
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-4/logs/run2_log_sbpmy
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### owner attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log info run --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-5/logs -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-5/logs/run1_log_UKkHY
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/owner/attempt-5/logs/run2_log_HSc4C
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### harness attempt 1/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/logs/run1_log_KJGNN
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-1/logs/run2_log_Iegjf
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### harness attempt 2/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/logs/run1_log_IAEgs
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-2/logs/run2_log_i4PZa
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### harness attempt 3/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/logs/run1_log_95XBh
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-3/logs/run2_log_ohf8B
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### harness attempt 4/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/logs/run1_log_WKLrQ
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-4/logs/run2_log_DDyhI
:: Success: deterministic. Determinism verified.
pid=3 result=55
```

### harness attempt 5/5 — PASS

Typed verdict: `matched`; bitwise parity: `true`; INFO: `109/109`; command exit: `0`.

Command:

```text
env LC_ALL=C TZ=UTC HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/home XDG_CONFIG_HOME=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/xdg-config E2E_TMPDIR=/tmp/hermit-e2e E2E_FIXTURE_DIR=/home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/fixtures /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/release/hermit --log=info run --backend ptrace --strict --verify --verify-json /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/verify.json --keep-logs --verify-log-dir /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/logs --no-virtualize-cpuid --max-timeslice=disabled -- /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/program
```

Output:

```text
:: Run1...
:: Run2...
:: Comparing captured verification logs...
Logs contain 109 | 109 messages total
Logs contain 107 | 107 detcore-specific messages
Logs contain 109 | 109 INFO messages
Logs contain 86 | 86 DETLOG & scheduler COMMIT messages
Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly...
  Comparing INFO messages...

Done processing logs, no substantive differences found (109 | 109 INFO messages compared).
:: Verification logs retained:
::   run 1: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/logs/run1_log_255Ei
::   run 2: /home/newton/work/dev-hermit/worktrees/divergence/hermit/target/ignored/enabled-red-current-head-spot-checks/debugger-c_debuggee/harness/attempt-5/logs/run2_log_OGgTm
:: Success: deterministic. Determinism verified.
pid=3 result=55
```
