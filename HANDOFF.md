# HANDOFF

Task: Goal 5 concurrent-validate repeat, solo baseline, and remaining manifest-audit/checker-compilation timing.

Primary slot: `/home/newton/work/dev-hermit/worktrees/122-goal5`.

## Exact source coordinates

- Planned devbig030 clean repeat, read remotely immediately before the attempted authority query:
  - dev-hermit `origin/main`: `421d79c28564f4d75ffe0ec5eba4d5ae10793477`
  - Hermit `origin/main`: `52d0b4d9f44b8ee4d98e89aafce11338d4c9f186`
  - Reverie `origin/main`: `48d063d2baed3c50c5e42d6f23636e7a41666ef9`
- The TMPDIR launcher fix named by the owner is landed at dev-hermit `8d21db237`.
- Local manifest timing worktree: Hermit `d4d9fe5effe31a90c5a64238ce99fc5ddeea4710`, recorded Reverie pin `ab07a89239150df3726a036bee9f5e897893dfc1`, branch `hermit-122/goal5-manifest-timing`.
- Checker-sharing candidate: `/home/newton/work/dev-hermit/worktrees/122-pin-cache`, Hermit `bae4a34fec5bfe45dd8cad00adcd5b6ddb04abcc`.
- Removed-fix control: `/home/newton/work/dev-hermit/worktrees/122-pin-cache-control`, same Hermit `bae4a34fec5bfe45dd8cad00adcd5b6ddb04abcc`.

## Current state

No validate is running for this task. The devbig030 SSH control socket was live as PID 2266235 and ordinary `ssh -o BatchMode=yes devbig030.atn3.facebook.com ...` reached the host. The attempted authority read failed before admission because the remote non-login PATH could not find `rust-script`; no unit or run record was created.

The prior devbig014 measurement unit is inactive and ran no child. Its refusal log is `/home/newton/work/dev-hermit/worktrees/122-goal5/ignored/hermit-122/manifest-audits-systemd.log`; its record is `/home/newton/work/dev-hermit/ignored/validate/runs/validate-hermit-122-manifest-audits-d4d9fe5effe31.json`.

Uncommitted/unpushed work at checkpoint:

- This slot has staged timing-only changes in `ci/manifest-plan/src/bin/test-harness.rs`. The attempted commit had not completed when the reboot instruction arrived.
- `ignored/hermit-122/measure-manifest-audits.sh`, `launch-manifest-audits.py`, and `summarize-manifest-audits.py` are local ignored measurement helpers.
- The checker-sharing candidate has uncommitted edits in `ci/run-reverie-pin-check.sh` and `ci/run-with-reverie-dbt-budget-test.sh`; `bash -n`, `shellcheck`, and `git diff --check` pass, but real tests have not run.
- The removed-fix control has only the compiler-count test edit in `ci/run-with-reverie-dbt-budget-test.sh`; it is intentionally expected to refuse the identical-source reuse assertion.

## Next action

1. Preserve and push all three branches before further measurement.
2. On devbig030, invoke remote commands with `PATH=/home/newton/.cargo/bin:$PATH` so `ci-hub` can execute, then reread `validate-lock authority-status --json` and the queue.
3. Prepare clean launcher worktrees at dev-hermit `421d79c28564f4d75ffe0ec5eba4d5ae10793477` and one clean Hermit source checkout at `52d0b4d9f44b8ee4d98e89aafce11338d4c9f186`; link each launcher's `ignored` directory to the canonical parent `ignored` directory as the earlier successful pair did.
4. Run one solo full validate first, with no other box work, and record its unit, log, start/end, wall time, trailing summary, retries, Hermit SHA, Reverie SHA, kernel, and capability set.
5. Then launch two full validates at those same coordinates and launcher conditions, confirm both units are active and both slot authorities name them simultaneously, and retain a one-second overlap recorder. Compare each arm against the solo baseline.
6. Return to the manifest timing run and checker-sharing positive/removed-fix controls only after the clean concurrent repeat.

Do not report any current validate handle for this task: none exists. Nothing from this task is unsafe to interrupt.
