---
title: 'validation-overhaul: finish full validation in under ten minutes'
status: closed
priority: 0
issue_type: epic
labels:
- validation
- performance
created_at: 2026-09-25T04:10:29.740062972+00:00
updated_at: 2026-09-25T15:31:36.491255619+00:00
closed_at: 2026-09-25T15:31:36.491255619+00:00
---

# Description

[gpt-5.6-sol] Audit and rebuild validation so affected-change runs are sharply selective while the complete portable, current-toolchain regression contract executes as a real dagrun graph with measured, justified parallelism.

# Design

One source of truth will classify paths, compute dependency closure, and assemble tool-owned DAG fragments. dagrun gains strict load-time includes with namespaces; execution profiles and per-test timings drive resource choices.

# Acceptance Criteria

A clean full validation run completes in under 10 minutes on the target host; selective documentation and single-tool edits complete substantially faster; every executable gate and every `py/` pytest case is inventoried and ranked by measured time, while other runner cases are attributed to a measured owning gate; expensive coverage receives a retain, optimize, relocate, or remove rationale; local and CI planning share one source; unknown paths fail safe; all changes are validated and pushed.

# Outcome

[gpt-5.6-sol] Closed at source commit `e6a545e93917bd245b2512b0eed99954798349b7`. Full run `18d89844a629049200077887` passed all 91 executable gates in 407.988 seconds, 192.012 seconds inside the ten-minute target, with 2,995.912 aggregate CPU-seconds and no positive memory-limit event. The source-stable v2 census passed all 5,882 Python cases and ranks every case; the v2 gate report ranks all 91 gates. The canonical graph, fail-closed selector, fragment includes, measured caps, and prior 1.5-second documentation / 14.76-second component selection proofs satisfy the delivered scope. Remaining telemetry, aggregate cgroup lanes, and process-generation safety are explicitly tracked by #27 cross-runner-test-timing, #28 delegated-cpuset, #29 delegated-scope-smokes, #30 pidfd-process-ownership, and #31 late-fork-census.
