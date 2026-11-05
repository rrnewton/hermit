---
title: 'validation-host-isolation: tests share host-global locks and guard roots, so validate-all fails at random'
status: closed
priority: 1
issue_type: bug
assignee: opus-5.5/validation-host-isolation
labels:
- agentctl
- wrkslots
- validation
created_at: 2026-09-26T04:56:31.551575650+00:00
updated_at: 2026-09-26T06:27:30.792557000+00:00
closed_at: 2026-09-26T06:27:30.792557000+00:00
---

# Description

[opus 5.5] Three unrelated failures on consecutive `validate-all` runs of one otherwise green
candidate (a vibe-talk-only stack on a547484). Each run failed in a different test, outside the
change. Two failures are reproduced deterministically and share one defect class: **a test uses
real host-global state, a fixed path under the host `/tmp`, as though it were private**. Any
concurrent holder of that state then breaks the test. The holder can be another test in the same
DAG, another checkout's validation, or a real agent. The third failure is not reproduced. Its
refusal message hides the real git failure, so the next occurrence cannot be diagnosed either.

Line numbers are at a547484. agentctl, wrkslots, scripts and the Makefile are identical in the
failing candidate.

## 1. agentctl: `concurrent_senders_serialize_within_and_across_queue_roots` (host-wide target lock)

Observed: `rust.agentctl.test`, 287 passed, 1 failed, panicking at `agentctl/src/agent.rs:2355`
on `assertion failed: gate.wait_for_entries(1, Duration::from_secs(5))` after 5.00s.

Root cause: `target_lock_path` (`agent.rs:1326`) always resolves to the real host-wide directory
`/tmp/herdr-agent-target-locks-<uid>/<sha256(pane identity)>.lock`, and it has no injection
point. The test helper `target()` (`agent.rs:2242`, used 29 times) always names pane `w1:p1`. So
every test in every checkout, and any real use of pane `w1:p1` on the host, contends for the same
file lock. The first sender blocks on it and never reaches the gate. The Python client already
isolates this: `py/tests/test_herdr_agent.py:38` monkeypatches `_target_lock_path` per test. Rust
has no equivalent.

Deterministic reproduction, from `rs/`:

    cargo test -q --locked -p agentctl --lib -- --exact \
      agent::tests::concurrent_senders_serialize_within_and_across_queue_roots      # passes 3/3
    D=/tmp/herdr-agent-target-locks-$(id -u)
    L=$D/$(printf '%s' '{"kind":"pane","pane_id":"w1:p1"}' | sha256sum | cut -d' ' -f1).lock
    flock -x "$L" sleep 8 &      # any concurrent holder
    cargo test ... (same test)   # fails at agent.rs:2355 in 5.00s, identical to the validate log

The digest is `pane_lock_digest("w1:p1")` (`agent.rs:988`), `a176b65b…d38c`.

## 2. wrkslots: `test_validation_exclusion_fixture_preserves_host_root_boundary` (guards in host `/tmp`)

Observed: ERROR at teardown in a `python-lifecycle.namespace` shard, `test_lifecycle.py:664`,
`assert all(guard.parent == root for guard in created)`. In that run a `python-lifecycle.host`
shard started immediately before the namespace shard, inside the same DAG.

Root cause: in the mapped-namespace branch, the test (`test_lifecycle.py:5800`) points
`_OWNERLESS_VALIDATION_EXCLUSION_ROOTS` at the host `/tmp` before calling
`install_validation_exclusion_fixture` (`:598`). The fixture then observes
`(/tmp, fixture root)`. At teardown it attributes to itself **every**
`.wrkslots-ownerless-validation.*` entry that appeared in the host `/tmp`. Meanwhile
`prepare_frozen_validation_checkout` (`:2386`, 35 callers) sets the roots to `(Path("/tmp"),)`.
So the frozen-validation tests, which are `ordinary_environment` and run in the host shards
concurrently with the namespace shards, create real guards directly in the host `/tmp`. Any one
of them alive across this test's teardown fails it. So does another checkout's validation, or a
real operator guard.

Deterministic reproduction, from `py/`, using a 12-line pytest plugin. Its
`pytest_runtest_teardown` hookwrapper creates `/tmp/.wrkslots-ownerless-validation.concurrent-repro-<pid>`
before the teardown and removes it afterwards, standing in for the concurrent frozen test:

    PYEXE=$(python3 -c 'import os,sys;print(os.path.realpath(sys.executable))')
    PYTHONPATH=<plugin dir> unshare --user --map-root-user --pid --fork --mount-proc "$PYEXE" \
      ../scripts/pid_namespace_init.py -- python3 -m pytest -q -p no:cacheprovider \
      -p concurrent_guard -c pyproject.toml --rootdir=. \
      'wrkslots/tests/test_lifecycle.py::test_validation_exclusion_fixture_preserves_host_root_boundary'
    # ERROR at teardown, test_lifecycle.py:664, identical to the validate log.
    # Without -p concurrent_guard: 1 passed.

## 3. wrkslots: `test_sidecar_cleanup_preserves_canonical_path_recreation` (not reproduced; diagnostics hide the cause)

Observed, in a `python-lifecycle.namespace` shard of a serial run (no overlapping DAG):
`code == 3` held, but stderr was
`REFUSED: checkout must be on its registered branch, not detached: <tmp_path>/...` instead of
`preserved both identities` (`test_lifecycle.py:20482`). The refusal came from
`_GitVcs.branch` (`wrkslots/cli.py:9019-9022`) during `remove`, before the sidecar quarantine.

What is ruled out:
- wrkslots never detaches a checkout (no `--detach` anywhere in `cli.py`);
- `_run` isolates git configuration (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_*` stripped), so shared git config is not the cause;
- test order: the same shard composition passed on the next two runs;
- load: the test passed 6/6 alone in the mapped namespace and 96/96 at 24-way parallelism
  under the same launcher, with a probe on `_GitVcs.branch` that never saw a non-zero
  `symbolic-ref`;
- the shard basetemp is a unique `mkdtemp` (`scripts/run_pytest_shard.py:176`);
- the kernel log shows no OOM kill in the window.

The defect that makes this undiagnosable: `branch()` runs `git symbolic-ref --quiet --short HEAD`
with `check=False` and reports **every** non-zero exit as "detached". With `--quiet`, a genuinely
detached HEAD exits 1 with empty stderr. Any other failure (exit 128 with `fatal:`, a signal, a
vanished or rewritten `.git` file, a concurrent writer) is misreported as a branch-state
condition, and git's stderr and exit status are dropped. Remaining hypothesis: a process outside
the test touched the checkout under `/tmp/agent-utils-pytest-*`, for example a host-level sweep
of `/tmp/agent-utils-*` checkouts or a kill of a reused PID. Nothing in the tree has been shown
to do that.

## Acceptance criteria

1. **agentctl lock root is injectable and private in tests.** The host-wide target lock root can
   be overridden, through a test-only constructor argument, a field on the queue context, or an
   environment override honoured only under `cfg(test)`. Every agentctl test, including the 29
   `target()` users, runs against a private per-test directory. Production still uses
   `/tmp/herdr-agent-target-locks-<uid>` unchanged.
2. **A deterministic isolation test for (1):** a test holds `flock -x` on the real host-wide
   lock file for pane `w1:p1` (or the injected root's equivalent from a sibling test) and proves
   `concurrent_senders_serialize_within_and_across_queue_roots`'s path still completes. Also
   prove that production resolution of `target_lock_path` still yields the host-wide path.
3. **The namespace guard test observes only its own guard.** Either
   - the mapped-namespace branch uses a private root it creates. Inside the user namespace a
     directory made by the mapped root is uid 0, so it can be mode 1777 on the same mount as
     the fixture; or
   - teardown attribution is restricted to the guard name this fixture's config and fenced path
     produce (`_ownerless_validation_exclusion_name`), not to every
     `.wrkslots-ownerless-validation.*` in the root.

   The boundary the test exists to check, that a guard never appears outside its root, must
   still be asserted.
4. **The frozen-validation tests stop leaving foreign-attributable guards in the host `/tmp`**
   where a private root is possible. Where a real root-owned sticky directory is required
   (ordinary environment, non-root), their guard names must be distinguishable by fixture, so
   no other test can mistake one for its own.
5. **A deterministic isolation test for (3)–(4):** the concurrent-guard injection above,
   expressed in-tree (a fixture that creates a foreign `.wrkslots-ownerless-validation.*` entry
   in the host root across the teardown), passes.
6. **`_GitVcs.branch` distinguishes detachment from git failure.** Exit 1 with empty stderr stays
   the "detached" refusal. Any other outcome refuses with the exit status and git's stderr, e.g.
   `git symbolic-ref failed (exit 128): fatal: ...`. A deterministic unit test covers both
   paths: a real detached worktree, and a checkout whose `.git` pointer is made invalid.
7. **Audit the same pattern elsewhere.** Other `check=False` git probes whose failure is mapped
   to a specific state get the same treatment, or a note saying why that mapping is exact.
   Other agentctl/wrkslots tests that take a fixed path under the host `/tmp` get a private root.
8. `make validate-all` is green, and the three reproductions above are recorded as passing in
   the closure comment. The closure names the sidecar cause if the new diagnostics identify it,
   or says plainly that it is still unidentified.
