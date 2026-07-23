---
name: progress-rubric
description: "Create evidence-based Hermit progress reports from live measurements. Use when gathering, validating, or writing a project progress report."
---

# Progress Report Rubric

Reusable template and procedure for Hermit progress reports
(`ai_docs/progress-reports/vN-YYYY-MM-DD.md`). The governing rule: **every number
is a live measurement**. Never estimate. If a suite cannot run, record the
exact reason (missing target, missing submodule, host capability gap, compile
error) instead of a number.

`scripts/progress-report.sh` automates this rubric end-to-end and writes a dated
report. Use it first; fall back to the manual steps below when you need to
investigate a specific suite.

## Required sections

1. **Test context** — commit SHA (full + short), branch, pull result, date
   (UTC), backend (`ptrace`/`DBI`/`KVM`), host CPU, kernel,
   `perf_event_paranoid`, toolchain versions, guest runtimes present.
2. **Host limitations encountered** — CPUID faulting availability, PMU access,
   missing runtimes/submodules. State each as an observed fact (quote the WARN
   or error).
3. **Summary table** — one row per suite: command, passed, failed, ignored,
   status.
4. **Per-suite detail** — one subsection per suite (below), each stating
   backend, log level, and relaxations (per AGENTS.md "Required Run Context").
5. **Recently landed PRs** — from `git log` on `main`.
6. **Known blockers / follow-ups** — root cause per failure, with file:line and
   verbatim error.

## How to gather each number

### Test context
```bash
git rev-parse HEAD; git rev-parse --short HEAD
with-proxy git pull origin main            # record "Already up to date" or the error
uname -r; grep -m1 'model name' /proc/cpuinfo
cat /proc/sys/kernel/perf_event_paranoid
rustc --version; cargo --version; cargo nextest --version
for t in python3 node redis-server sqlite3 java; do command -v "$t" || echo "$t MISSING"; done
```

### Strict / fail-closed ratchet
```bash
./scripts/test-fail-closed.sh 2>&1 | tee /tmp/progress-strict.log
```
- Sets `HERMIT_FAIL_CLOSED=1`; strictest mode, relaxations = none.
- **Fail-fast** (`set -e`): a single failure aborts before later targets. If it
  aborts, report the tests that ran (grep `==> Fail-closed:` and `test result:`)
  and note that the final ratchet line is absent.
- Green run ends with: `Fail-closed ratchet passed: N enabled, K known failures,
  I ignored, M mode N/A.` — copy those exact counts.
- Known-failure allowlist: `hermit-cli/tests/fail_closed_known_failures.tsv`;
  allowed ignores: `hermit-cli/tests/fail_closed_allowed_ignores.tsv`. A failure
  absent from the first is unexpected.

### Working-envelope vector (L1–L4 + rr) — canonical assurance measurement
```bash
./validate.sh --envelope-only 2>&1 | tee /tmp/progress-envelope.log
cat envelope.json     # gitignored; do NOT commit
```
Reports `l1_pass..l4_pass`, `rr_pass`, `total` over the `ENVELOPE_PROBES` list.
Assurance ladder (AGENTS.md): L1 `--strict`; L2 `--strict --verify`; L3 adds
`--detlog-heap --detlog-stack`; L4 = L2 ×`L4_REPS` (default 20); rr =
`record start --verify` end-to-end. Monotonicity can be gated with
`./validate.sh --envelope-compare baseline.json`.

### rr suite
- **There is no `rr_suite` Cargo target and no `third-party/rr` submodule** in
  the OSS repo. Meta's Buck rr matrix is not ported (AGENTS.md → "Test"). Report
  this explicitly; do not invent numbers. OSS rr coverage = envelope `rr` probes
  + the `record_replay` target.

### Record / replay
```bash
cargo test -p hermit --test record_replay 2>&1 | tee /tmp/progress-record.log
```
The target is `record_replay` (not `record`). Copy the `test result:` line.

### App end-to-end suites
- **There is no `app_strict_verify` target.** Run the per-app targets that
  exist:
```bash
for t in sqlite_veryquick redis_strict python_stdlib language_runtime_determinism; do
  cargo test -p hermit --test "$t"
done
```
- Default run = non-ignored only. Ignored tests need `-- --ignored` plus
  optional downloads/toolchains (SQLite/Redis source builds, CPython Lib/test,
  go/jvm/node/ocaml/python/ruby). Record passed/failed/ignored per target and
  name the active passing tests.

## Rules

- Report backend, log level, and relaxations for every Hermit run.
- Quote verbatim errors with `file:line`. Separate host limitation from product
  regression; if you cannot tell, say so and say what host would decide it.
- Do not stage generated artifacts (`envelope.json`, `target/`) or unrelated
  concurrent changes.
- Reuse existing paths: reports in `ai_docs/progress-reports/`, this rubric in
  `.llms/skills/`, automation in `scripts/progress-report.sh`.
