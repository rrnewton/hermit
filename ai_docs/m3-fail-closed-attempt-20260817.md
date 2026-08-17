# M3 fail-closed attempt, 2026-08-17

## What M3 means

The operative definition is in the archived development arc:

> **M3 — Full Sanity** | M2 verification passes while unsupported syscalls fail closed by default. | Flip run and record/replay to fail closed, implement the syscalls needed by real tests, and ratchet the score back upward. A second score drop may occur.

The same document expands the requirement:

> M3 is not “turn on one boolean and accept the damage.” It requires:
>
> - ordinary run to fail closed by default;
> - record and replay to fail closed too;
> - ptrace and other backends to actually route relevant syscalls through Detcore rather than bypassing it;
> - tests that use unsupported syscalls to be repaired by implementing the required support, not by reclassifying them as PassThrough or weakening the test;
> - a fresh score measured under the new default.

Source: `ai_docs/archived/2026-08/sanity-milestones-development-arc-20260816.md`, lines 27-31 and 167-182 as read on 2026-08-17. The same snapshot records one explicitly Unsupported syscall, `restart_syscall`, at lines 506-512. Reclassification, comparator changes, exclusions, and weaker assertions are outside this attempt.

## Candidate

- Hermit branch: `sanity-driver/m3-fail-closed-attempt`
- Hermit commit: `a812a09bb10f1362391689808153b8f49fff1467`
- tested base: `d56112e9b6ac67a1d3c839f7239a0e09755f428f`
- staged Reverie dependency: `e7972364634aae3ef62705527c70a1c0556c5784`
- exact Hermit binary SHA-256: `5275d1c24a201155168278a0b4528e83159fc426c75a78f91659a07023ce972f`

The candidate makes ordinary run, recording, and replay fail closed for the explicit Unsupported class. The ptrace backend dependency makes `restart_syscall` subscription-controlled, and the reduced subscription set includes Unsupported syscalls. Ordinary run retains an explicit warning-producing `--allow-unsupported-syscalls` compatibility opt-out. Record and replay have no opt-out.

This candidate must not land as-is: the Reverie dependency is not contained in `rrnewton/reverie:main` as observed on 2026-08-17. That fact is invalidated if Reverie main later contains `e7972364`'s content or a reviewed equivalent.

## Current green denominator

The authoritative checked-in table contained 170 green cells:

```text
jq '[.cells[] | select(.status=="green")] | length' ci/compat-envelope/cells.json
170
```

The measurement covered all 170 identities: 169 portable comparable cells and the one privileged `backend-parity-c/cpuid-probe / verify / ptrace` cell. Two custom portable rows also ran and passed, but are not in the 170-cell denominator.

## Result

Worst-case current-green loss was **11/170 cells (6.47%)**, below the owner's staging threshold of 17 cells (10%). The candidate therefore meets the numeric gate without relying on a promotion.

The 170 current-green identities produced:

| Result | Cells | Fraction of 170 |
| --- | ---: | ---: |
| canonical pass | 159 | 93.53% |
| incomplete DBT verification evidence | 9 | 5.29% |
| canonical ptrace divergence | 2 | 1.18% |

No non-passing row reported an unsupported-syscall refusal. Therefore **0/170 losses are attributable to M3 fail-closed exposing a previously fail-open cell in this run**.

The 11 non-passing rows are:

| Cell | Result | Classification |
| --- | --- | --- |
| `backend-parity-c/host-identity / verify / dbt` | ERROR: no comparison report | infrastructure evidence; guest and DBT run exited successfully, not an unsupported-syscall refusal |
| `c-programs/add-key-enosys / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/cachestat-enosys / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/futex-waitv-enosys / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/get-robust-list-self / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/ioctl-fioclex / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/kcmp-eperm / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/keyctl-enosys / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `c-programs/listmount-enosys / verify / dbt` | ERROR: no comparison report | infrastructure evidence; not an unsupported-syscall refusal |
| `determinism-stress/order-violation / chaos / ptrace` | FAIL: seed 9 diverged; 31 other seeds matched | canonical determinism divergence, not an unsupported-syscall refusal |
| `language-runtimes/node-v8-jit / verify / ptrace` | FAIL: INFO comparison diverged | canonical determinism divergence at wait4/polling, not an unsupported-syscall refusal |

The nine DBT rows are the complete nine-cell DBT portion of the checked-in green table. Each process returned status 0 but published `verified=false`, `verdict=no_result`, no comparison, and no compared INFO counts. They are not product failures caused by M3, and they are not admissible canonical evidence.

## Fail-closed brackets

The candidate was also tested directly in both directions:

- ordinary ptrace run: supported `/bin/echo` succeeds; default `restart_syscall` fails and names the syscall; the explicit compatibility opt-out succeeds and prints both warnings — 1 test passed;
- ordinary DBT run: supported `/bin/echo` succeeds; default and strict `restart_syscall` fail and name the syscall; the explicit compatibility opt-out succeeds and reports the unsupported syscall exactly once, including fork/exec and tamper brackets — 1 test passed;
- recording: `restart_syscall` fails, names the syscall, does not hang, and does not publish the old success marker;
- replay: a supported recording whose same-length argument is changed to take the `restart_syscall` branch fails, names the syscall, does not hang, and does not publish the old success marker — 2 tests passed;
- reduced subscriptions include every explicitly Unsupported syscall — 1 test passed;
- run option default/opt-out and record/replay configuration unit brackets — 2 tests passed.

No assertion, comparator, tolerance, timeout, result classification, or scorecard status was weakened.

## Commands and retained evidence

The Rust manifest runner executed the same `CellRunSpec`, per-cell executor, and canonical predicate used by validation. The portable cells were run sequentially in the owned worktree after two fresh-checkout pressure attempts were prevented from reaching any cell by host BPF compiler-write denials. The one privileged cell was then run separately. This is complete per-cell evidence, but not a completed pressure DAG.

- portable schema-4 results: `ignored/e2e-m3-a812a09/results-escalated.jsonl`
  - SHA-256: `ae7cd31d0bc67741274a1a4c95f2a18b9be30686923cd1a7fa31223cfd5c3c55`
- privileged schema-4 result: `ignored/e2e-m3-a812a09/results-privileged.jsonl`
  - SHA-256: `aae6d30ac488ca03c06d1671c69fda3c1d17361aca02fd7ae5ca6dd3a816e225`

Key commands:

```text
target/debug/test-harness build --lane portable --ci-only --allow-empty
target/debug/test-harness run --lane portable --ci-only --prebuilt --results ignored/e2e-m3-a812a09/results-escalated.jsonl
target/debug/test-harness build --lane privileged --ci-only --allow-empty
target/debug/test-harness run --lane privileged --test backend-parity-c/cpuid-probe --mode verify --backend ptrace --ci-only --prebuilt --results ignored/e2e-m3-a812a09/results-privileged.jsonl
cargo test -p hermit --features third-party-backends --test cli run_ptrace_fails_closed_by_default_on_unsupported_syscall -j1 -- --nocapture
cargo test -p hermit --features third-party-backends --test cli run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls -j1 -- --nocapture
cargo test -p hermit --features third-party-backends --test record_replay unsupported_syscall_by_name -j1 -- --nocapture --test-threads=1
```

This report becomes stale if the checked-in green population changes, the M2 comparator changes, the Unsupported classification changes, the candidate is rebased, or the Reverie dependency changes. Recompute the 170-cell denominator and rerun the exact identities in those cases.
