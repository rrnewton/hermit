# Validation overhaul audit

Date: 2026-09-25 UTC

This is working evidence for #21 validation-overhaul, not settled user-facing documentation. It
records the implemented validation architecture, the current complete timing census, the measured
parallelism choices, and the remaining work that was deliberately split into follow-up issues.

## Outcome

The portable current-toolchain contract now runs as one composed dagrun graph. At source commit
`867aa25d80c0a510529a42e19614736139aab648`, full run
`18d8a746d8b9efeb003f47c3` completed all 92 executable gates successfully in 436.775 seconds
(7 minutes 16.775 seconds). That is 163.225 seconds below the ten-minute target. The run recorded
3,570.372 aggregate step-wall seconds, 3,278.946 aggregate CPU-seconds, and no step with a positive
`memory.events` maximum.

The current Python census is independently complete and green: all 5,906 collected cases passed,
none skipped, with 1,260.981 seconds of summed testcase time. Exact collection equality, disjoint
phase membership, native pytest node identities, artifact hashes, and the source-stable snapshot are
recorded in `test-timing-provenance.json`. Every row has an ownership, coverage-contract, and
recommendation classification in `test-timing-ranking.tsv`.

## Evidence boundary

The gate report is schema `agent-utils-validation-gate-ranking/v2`. Its provenance binds the 92
profile rows to the canonical root DAG, the expanded DAG, the public gate listing, the profile run,
and source commit `867aa25d80c0a510529a42e19614736139aab648`. The generator refuses missing or duplicate gates,
mixed run metadata, failures, timeouts, positive OOM evidence, graph drift, or source drift. The
ranking's SHA-256 is recorded in the provenance.

The test report is schema `agent-utils-validation-test-timing-provenance/v2`. The capture began and
ended at the same source snapshot, with zero tracked source-diff bytes outside its generated
artifacts, zero bound untracked files, and no generated JUnit input present before collection.
Fresh live collection, not pytest's accumulating cache, defines completeness. All 5,906 JUnit cases
carry exact `nodeid` and `family` properties; legacy classname reconstruction was not used. The
5,906-row TSV has 13 columns and its SHA-256 is recorded in the provenance.

JUnit `time` is elapsed testcase time, including fixture work. It is not CPU time. Dagrun profiles
supply step wall time, user plus system CPU, peak memory, caps, throttling, pressure, and memory
events. This remains a one-host census and should be treated as a scheduling seed rather than a
low-variance benchmark.

## Implemented architecture

### One graph, composed at load time

`validation.dag.yaml` is the single portable current-toolchain contract. It includes ten focused
fragments: repository checks, ordinary Python components, Python lifecycle tests, Rust tests,
cross-language tests, examples, Python packages, Rust packages, timeline/browser tests, and
vibe-talk. One outer dagrun invocation schedules the flattened graph, so independent languages and
tools overlap under one resource model instead of running as serial phases. Selected validation and
full validation load the same graph; selection filters labels rather than defining another contract.

DAG inclusion is transparent compile-time composition, not a runtime subgraph node. Each fragment
is flattened under its namespace, nested namespaces compose with dots, and all internal dependency
references are rewritten to their effective names. An include's optional `after` dependencies are
resolved in the parent's namespace and fanned into every entry node of the included subgraph. An
outer step can connect to one exact internal node by depending on its fully namespaced tag.

There is deliberately no implicit whole-subgraph completion edge. If a fragment must appear as one
node to downstream work, it must declare an explicit terminal barrier step that depends on every
intended exit; the outer graph then depends on that barrier's exact namespaced tag. This makes the
boundary visible and reviewable, while allowing callers that need an internal connection point to
name one without waiting for unrelated exits.

The loader fails before planning on malformed or unknown include fields, absolute, missing, or
escaping paths, namespace collisions, cycles, duplicate effective includes, policy conflicts,
invalid `after` targets, or expansion-limit violations. The reserved `include` key is rejected by
path-free parsing and standard input, where no safe base directory exists; it is never silently
ignored. Python and Rust share parity fixtures for these rules.

### Fail-closed selection and ownership

`scripts/validate.py` gathers committed, unstaged, and untracked paths. Rename detection is disabled
so both the removed and added paths contribute. Broad path rules select groups, while
`validation/components.json` owns 185 ordinary Python test files across nine components and seven
separate-suite files, including the lifecycle partition. Its self-test rejects unclassified,
missing, or multiply owned test files.

An unknown path selects the complete portable contract, and a multi-area change selects the union
plus reverse dependencies. Repository hygiene is always selected. A component-only edit selects
that component's Python, Rust, cross-language, and packaging nodes plus declared dependents;
`--all` executes the complete flattened graph. Local validation and CI therefore consume the same
plan rather than maintaining copied command lists.

### One owner for nested execution

Most gates are ordinary children of the outer scheduler. Tests that intentionally launch dagrun
again must opt in with `delegated_children: true`. Under cgroup v2 the outer run retains the entire
subtree, places the command in a supervisor leaf, and delegates an empty child root to the inner
scheduler. Cancellation therefore reaches descendants even if they leave their original process
group. An explicitly unboxed outer run passes only a narrowly scoped fallback marker to opted-in
steps and labels the degraded procfs/process-group accounting path.

Python tests are split into deterministic, disjoint component shards with separate temp roots. The
destructive lifecycle suite has six mapped-user/PID-namespace shards and three host-visible shards.
Cargo consumers share a repository target-directory cap. The outer planner may dispatch at most 24
steps using 32 CPUs, while resource caps and measured inner widths constrain actual overlap.

## Current Python test ranking

### Complete census

| Phase | Cases | Result | Sum of shard-suite time | Sum of testcase time | Testcase-time share |
|---|---:|---|---:|---:|---:|
| General | 4,767 | 4,767 pass, 0 skip | 528.230 s | 524.950 s | 41.630% |
| Lifecycle, namespaced | 857 | 857 pass, 0 skip | 499.633 s | 498.032 s | 39.496% |
| Lifecycle, host-visible | 282 | 282 pass, 0 skip | 238.786 s | 237.999 s | 18.874% |
| Total | 5,906 | 5,906 pass, 0 skip | 1,266.649 s | 1,260.981 s | 100% |

Cost remains concentrated but less extremely than in the first census: 255 cases account for 50%
of testcase time, 757 for 80%, 1,097 for 90%, 1,425 for 95%, and 2,637 for 99%.

| Component | Cases | Testcase seconds | Share |
|---|---:|---:|---:|
| wrkslots | 1,370 | 804.394 | 63.791% |
| dagrun | 954 | 224.501 | 17.804% |
| repository-infrastructure | 188 | 79.218 | 6.282% |
| agentctl | 1,647 | 73.888 | 5.860% |
| wrkviz | 944 | 72.003 | 5.710% |
| herdr-run | 423 | 3.232 | 0.256% |
| tick-hub | 133 | 2.345 | 0.186% |
| planner | 142 | 0.826 | 0.066% |
| experiment-runner | 105 | 0.574 | 0.046% |

### Highest current testcase costs

These are the first twelve rows of the complete TSV, without extrapolation:

| Seconds | Phase | Component | Test | Coverage contract | Recommendation |
|---:|---|---|---|---|---|
| 36.856 | lifecycle-host | wrkslots | `test_remove_refuses_live_process_using_slot` | process-liveness | keep-host-sharded |
| 20.626 | general | dagrun | `test_unboxed_run_enforces_a_lower_bound_and_exposes_its_escape` | dag-scheduler-enforcement | keep-profile-optimize |
| 17.954 | general | wrkslots | `test_real_process_and_git_invariants` | wrkslots-accounting | keep-isolated |
| 14.947 | general | wrkslots | `test_lsof_guard_refuses_a_real_live_user_with_attributed_evidence` | wrkslots-accounting | keep-profile-optimize |
| 13.660 | general | dagrun | `test_default_small_cpu_cap_is_enforced_and_allows_compliant_work` | dag-scheduler-enforcement | keep-profile-optimize |
| 11.108 | general | dagrun | `test_delegated_nested_run_keeps_descendants_in_outer_owned_subtree` | dag-scheduler-enforcement | keep-profile-optimize |
| 10.295 | general | repository-infrastructure | `test_sigterm_stops_rsync_and_seals_an_interrupted_receipt` | repository-infrastructure | keep-harness-isolated |
| 8.651 | general | dagrun | `test_an_outer_budget_cut_names_the_budget_and_never_a_peer` | dag-scheduler-enforcement | keep-profile-optimize |
| 7.855 | general | dagrun | `test_peer_cancellation_survives_a_later_deadline_in_the_same_run` | dag-scheduler-enforcement | keep-profile-optimize |
| 7.346 | general | agentctl | `test_command_timeout_contains_group_when_supervisor_is_stopped[emergency]` | agent-lifecycle-chat-state | keep-profile-optimize |
| 7.175 | general | agentctl | `test_command_timeout_contains_group_when_supervisor_is_stopped[census-error]` | agent-lifecycle-chat-state | keep-profile-optimize |
| 6.643 | general | dagrun | `test_outer_run_budget_cuts_a_long_run_early_and_still_reports` | dag-scheduler-enforcement | keep-profile-optimize |

All 5,906 rows, including parameter-family identity and cumulative share, are in
`test-timing-ranking.tsv`. The recommendation totals are also complete: 4,718 `keep-targeted`, 857
`keep-namespace-sharded`, 281 `keep-host-sharded`, 33 `keep-harness-isolated`, 11
`keep-profile-optimize`, three `reduce-bounded-iterations`, and one each of `keep-isolated`,
`keep-focused-proc`, and `shorten-test-only-deadline`. These categories preserve expensive safety
anchors while identifying setup, bounded-loop, wait, isolation, and selection opportunities. Raw
duration alone is not grounds to remove destructive Git, process-liveness, cgroup, crash-recovery,
or fail-closed coverage.

Per-case timing for non-pytest runners remains #27 cross-runner-test-timing. Until that lands,
Rust/libtest, JavaScript/browser, cross-language, package, example, and application cases are
accounted for by their measured outer gates without pretending that gate duration is a case timing.

## Current full-graph gate ranking

`gate-timing-ranking.tsv` contains every one of the 92 executable gates. It ranks wall and aggregate
CPU time and records peak memory, applied cap, memory events, coverage rationale, and a retain or
optimization recommendation. The ten highest aggregate-wall gates are:

| Rank | Gate | Wall | CPU | Peak MiB | Cap MiB |
|---:|---|---:|---:|---:|---:|
| 1 | `cross.dagrun.differential` | 432.399 s | 509.206 s | 227.77 | 2,560 |
| 2 | `vibe-talk.shots.screenshots` | 285.314 s | 176.312 s | 659.90 | 3,072 |
| 3 | `rust.dagrun.test` | 212.443 s | 135.458 s | 1,479.30 | 7,680 |
| 4 | `cross.agentctl.differential` | 192.955 s | 171.605 s | 1,747.45 | 3,200 |
| 5 | `rust.agentctl.test` | 157.312 s | 120.170 s | 134.51 | 7,680 |
| 6 | `python-lifecycle.host.shard-2` | 155.186 s | 148.317 s | 342.07 | 1,280 |
| 7 | `examples.run.rust` | 143.321 s | 106.761 s | 559.36 | 16,384 |
| 8 | `examples.run.python` | 138.685 s | 98.327 s | 571.93 | 16,384 |
| 9 | `python.dagrun.shard-0` | 137.282 s | 128.000 s | 675.09 | 2,048 |
| 10 | `python-lifecycle.namespace.shard-1` | 120.228 s | 114.474 s | 182.84 | 1,280 |

Nine gates account for 50% of aggregate gate-wall time, 19 for 80%, 26 for 90%, and 34 for 95%.
The dagrun differential occupied 432.399 of the 436.775 graph seconds (98.998% of the critical-path
window) while averaging 1.178 measured CPU cores. The complete graph averaged 7.507 measured CPU
cores against its 32-CPU ceiling. Outer-DAG concurrency is therefore no longer the principal limit;
the dominant remaining wall-time opportunity is to split or internally parallelize that
differential safely. The screenshot gate is expensive in aggregate time but overlaps the critical
path and retains real-browser coverage unavailable from unit tests.

All 92 gates reported zero `memory.events` maxima, so none of their recorded peaks is marked
censored by a memory ceiling. That is evidence for this run, not permission to remove caps.

### Aggregate-only live cgroup omissions

Green gate status does not mean every environment-specific live cgroup leg ran inside the
aggregate graph. A nested test that creates a fresh top-level systemd user scope can migrate into a
sibling unit and escape the outer gate's ownership, so those legs now skip loudly rather than
weakening containment:

- #28 delegated-cpuset covers live `pin-run` and `cpuset-alloc` apply/release, interop, and selftest
  execution beneath an already delegated parent.
- #29 delegated-scope-smokes covers forced-box, observed-live-PID, boxed-journal, Python symlink and
  stdin re-exec, and Rust boxed-stdin live scope legs in a containment-safe lane.

Their non-live schema, refusal, policy, and direct-route assertions still execute in the aggregate
contract, and the live legs remain runnable standalone on a capable host. The follow-ups are open
because an aggregate safety skip must not be mistaken for exercised coverage.

## Reliability and efficiency findings

The overhaul removed or isolated several costs that did not buy regression coverage: ambient stale
tmux-agent probes, a host-wide process scan inside a synthetic lifecycle race, fixed readiness
assumptions, unsafe shard temp-root sharing, duplicate nested validation phases, and serial
language/tool execution. Resource profiles also exposed under-sized learned memory limits,
publication races, and delegated-scope escape hazards; those now fail closed or have explicit
containment boundaries.

A full-graph run then exposed a procfs parser assumption that focused tests had missed: Linux
process names are opaque bytes and can contain invalid UTF-8, newlines, spaces, and parentheses.
Dagrun, agentctl, and wrkslots now parse `/proc/<pid>/stat` from bytes using the final validated
`) <state> ` delimiter and validate process identities. Status-field readers split only on literal
LF, so control bytes in `Name:` cannot forge `Uid`, `State`, `Cpus_allowed_list`, or `NSpid` lines.
Only definite disappearance (`ENOENT` or `ESRCH`) is treated as gone; malformed or unreadable live
identity evidence fails closed. Teardown reports incomplete global scans instead of declaring a
clean sweep, and wrkslots distinguishes harmless unrelated process churn from ambiguous evidence
for a retained candidate.

The remaining generation-safe signaling work is #30 pidfd-process-ownership, and the remaining
post-snapshot inheritance boundary in the same-UID census is #31 late-fork-census. Neither is hidden
inside the closed flake issue.

## Measured concurrency choices

The design-time sweeps below predate the current v2 census, but they remain the measured basis for
the checked-in caps and are retained here as historical calibration rather than current reruns.

### Lifecycle outer width

Each trial ran the then-current 1,130-case lifecycle suite exactly once and passed.

| Concurrent lifecycle nodes | Wall | CPU total |
|---:|---:|---:|
| 4 | 247.7 s | 730.9 CPU-s |
| 5 | 190.0 s | 743.0 CPU-s |
| 6 | 218.3 s | 748.9 CPU-s |

Width five was the fastest observation and used less CPU than width six, so `lifecycle-io` remains
capped at five. Host-visible work is additionally capped at three.

### Cargo inner width

Each historical trial used a clean dedicated target directory and was a cold full-workspace release
build.

| Cargo jobs | Wall | CPU total |
|---:|---:|---:|
| 1 | 263.884 s | 255.925 CPU-s |
| 2 | 130.638 s | 258.583 CPU-s |
| 4 | 69.552 s | 260.103 CPU-s |
| 8 | 43.230 s | 258.705 CPU-s |
| 16 | 41.527 s | 261.068 CPU-s |
| 24 | 45.097 s | 263.881 CPU-s |
| 32 | 41.591 s | 269.251 CPU-s |

Eight jobs is the chosen elbow: it was the smallest width within 5% of the fastest observation,
while larger widths gave no stable gain and consumed at least as much CPU.

## Selection evidence

The following selection measurements are historical proofs from the implemented graph before the
current 92-gate run; they are not presented as measurements at commit `867aa25`:

- A documentation-only change selected seven always-on hygiene nodes, took 1.0 seconds of graph
  time, and completed the selector/driver invocation in 1.5 seconds.
- A synthetic `py/tick_hub/cli.py` change selected 17 nodes in run
  `18d88315e12d6b26003e9226`, passed in 13.9 seconds of graph time and 14.76 seconds end to end, and
  consumed 42.246 aggregate CPU-seconds.

These measurements establish the intended order-of-magnitude advantage over the full contract;
selector self-tests and the canonical graph contract protect the behavior on the current tree.

## Historical baselines, explicitly superseded

The first timing census at commit `d893100730bb3a7f115724431c6c619be8164dfe` contained 5,734
distinct cases: 4,604 general, 848 namespace lifecycle, and 282 host lifecycle. It was not green:
15 general cases failed because of measurement-environment and readiness problems. Its summed
testcase time was 2,752.713 seconds; measured corrective substitutions produced a 2,410.090-second
planning estimate. Those values describe the original problem and must not be read as current
suite counts or performance.

An earlier green graph run, `18d885f0608c7bd9002d2cf9`, contained 78 gates and took 371.650 graph
seconds, 3,272.204 aggregate step-wall seconds, and 2,916.240 aggregate CPU-seconds. Its dagrun
differential took 368.428 seconds. The graph subsequently grew to 92 gates and the current evidence
is run `18d8a746d8b9efeb003f47c3`; the 78-gate values are retained only to preserve audit history.

## Open follow-ups

- #27 cross-runner-test-timing: normalize per-case timing beyond pytest.
- #28 delegated-cpuset: execute live hard-pinning legs under the owning delegated cgroup.
- #29 delegated-scope-smokes: preserve fresh top-level scope smokes in a containment-safe lane.
- #30 pidfd-process-ownership: bind agent cleanup to durable kernel process identity.
- #31 late-fork-census: close the same-UID post-snapshot inheritance race.
- #35 contained-example-memory: make the fixed 16 GiB example envelope portable to future small,
  cgroup-capable runners.

#21 validation-overhaul through #26 validation-flakes are closed by the current green full run,
complete rankings, selection contract, include implementation, and documented follow-up boundary.
