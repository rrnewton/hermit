---
title: 'abort-create-census-cost: recover --abort-create spends two minutes in a host-wide mount scan, and unit tests pay it'
status: closed
priority: 1
issue_type: bug
assignee: opus-5.5/abort-create-census-cost
labels:
- wrkslots
- validation
- performance
created_at: 2026-09-26T06:56:28.757636169+00:00
updated_at: 2026-09-26T07:41:36.611508000+00:00
closed_at: 2026-09-26T07:41:36.611508000+00:00
---

# Description

[opus 5.5] Found after #45 validation-host-isolation landed. The next serial `validate-all` of an
unrelated vibe-talk candidate failed in exactly one step. `python.wrkslots.test` (the non-lifecycle
wrkslots tests) was killed at its 600 s budget; its estimate is 150 s. No other DAG was running.
At the kill, dagrun saw a cpu-burning child:
`python3 -m wrkslots --project-root <pytest tmp>/create0/project recover ...` (40 s wall, 27 s cpu).
The step printed no test names, because `-q` names failures only at the end.

Line numbers are at 4d78fe6.

## Measured

A verbose isolated run of the same step passes, 313 passed in 357 s. The two slowest items are the
same operation, `recover --abort-create`:

| item | time |
|---|---|
| `test_scoped_journal_episodes.py::test_create_retried_after_abort_recovers_its_interruption` (call) | 46.4 s |
| `test_scoped_create_episodes_need_a_new_operation[new-operation]` (setup of the module fixture `aborted_create`) | 42.7 s |

The hung process's `create0` path is `tmp_path_factory.mktemp("create")`, the `aborted_create`
fixture (`test_scoped_journal_episodes.py:382`), not the function-scoped test. The fixture's own
comment says its abort "runs the host-wide slot-use census, which is too slow to repeat for every
shape". A second isolated run of the test alone took 123 s, so the cost tracks whatever else runs on
the host.

cProfile of that 123 s run's `recover --abort-create` subprocess, which took 119 s end to end:

    _cmd_recover → _recover_create → _abort_create                      118.9 s
      → _assert_slot_unused (cli.py:11384) → _observe_slot_use_once      118.2 s
        → _process_mounts_slot (cli.py:10948)   3,502 calls            111.1 s
          → _path_is_within (cli.py:2454)       915,709 calls          103.0 s  (pathlib.relative_to)

## Two defects

1. **Incidental real census in journal-retry tests.** Both call sites assert only journal and
   active-slot state. The census's verdict is not what they test. They run it unstubbed through
   `command(...)`, while neighbouring tests in the same file use
   `raw_command_with_census_authority_stub` (`test_lifecycle.py:885`). That is the same class as
   #45: a unit test depends on host-global state, here every process on the host, and its duration
   is set by other tenants.
2. **The production census is quadratic in pathlib.** `_process_mounts_slot` parses each process's
   `mountinfo` and runs `pathlib.Path.relative_to` against every mount line. Mount-namespace dedupe
   (`inspected_mount_namespaces`, around `cli.py:11576`) applies only on the
   `direct_use_proven_absent` branch. `_process_uses_slot` (around `cli.py:10896`) scans mounts per
   process with no dedupe. `_mount_namespace` also returns a unique `pid:<n>` key on
   `PermissionError`, which defeats the dedupe for processes the caller cannot inspect. On this host
   one `wrkslots recover --abort-create` takes about two minutes. That matters to operators, not
   only to tests.

## Not the cause, but seen in the same failed run

The one `F` printed before the kill is at position 163 of the fixed collection order. That maps to
`test_event_memo.py::test_validate_batch_parses_each_event_once_and_folds_each_event_once`, a
positional inference that is **unverified**. That test stubs the censuses
(`stub_validate_batch_censuses`) and passed in isolation. Its failure is unexplained. It is recorded
here because the timeout erased its name.

## Acceptance criteria

1. The two journal-retry call sites, the `aborted_create` fixture and
   `test_create_retried_after_abort_recovers_its_interruption`, run `recover` through the census
   authority stub (or an equivalent in-process stub). Their journal assertions are unchanged. At
   least one test still exercises the real `_assert_slot_unused` path for `--abort-create`,
   bounded and deliberate, not incidental.
2. `_process_mounts_slot` / `_process_uses_slot` do not repeat identical mount scans:
   - dedupe by mount namespace on every branch;
   - keep the `PermissionError` case sound, with no false "absent" for a namespace the caller
     cannot read, and without degrading to one scan per PID when the `mountinfo` content is
     identical, e.g. key on the content digest;
   - replace per-line `pathlib.relative_to` with a normalised string-prefix containment check that
     keeps the component-boundary semantics (`/a/b` is not within `/a/bc`), with a unit test for
     that boundary.
3. A deterministic cost test, e.g. N synthetic processes × M mount lines, bounds the work: path
   comparisons stay proportional to distinct mount tables, not to processes × lines.
4. The wrkslots component runner makes a failing test's node ID visible before a step timeout can
   erase it, for example a `pytest_runtest_logreport` hook that prints failing node IDs immediately.
5. **The 600 s step budget is not raised.** `python.wrkslots.test` on this host is back near its
   150 s estimate, and the closure records the measured time and the new profile of one real
   `recover --abort-create`.
6. `make validate-all` is green. The closure says whether `test_validate_batch_parses_each_event_once_and_folds_each_event_once`
   was reproduced, and why it failed if so.
