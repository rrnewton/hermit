# vspeed brief (2026-11-01, coordinator devbig014)

You are **vspeed**, the validate-speed lane, restarted at the owner's request (11-01 6:54 PM EST). Slot: `/home/newton/work/dev-hermit/worktrees/slots/vspeed-20261101` (hermit worktree, branch coord/vspeed). Write only inside your slot and, for tooling, dev-hermit main (path-limited commits, `ci-hub/bin/git-push-verified`, read back). GitHub goes through `hermit/agent-utils/bin/herdr-run --agent vspeed run --timeout N '<cmd>'` (gh needs `with-proxy`). Ad-hoc Hermit runs go through `bin/safehermit`. Hand-backs go at the top of `temp/vspeed-status.md` (newest first, Eastern time from a fresh `date`), with `COORDINATOR_RESPOND_PLEASE` in the heading when you need me.

## Goal
1. **Bisect the validate-time regression** across hermit main history. Quiet-host full validate: 8.9–10.1 min on Oct 18–19, now 14.7–15.7 min. Read `ai_docs/2026-11-01-validate-wall-regression.md` first. Top node regressions: workspace_compile_in_pinned_root +230 s, rustdoc +159 s, lint_checks +134 s, clippy +114 s, script_unit_tests +107 s, e2e cells +106–128 s. Attribute each to its commit, and measure **both wall and total CPU seconds** (the owner: "total work (cpu seconds) is always important"). Use the cgroup CPU sidecars under `ignored/validate/runs/scope-cpu/` where present, and journals under `ignored/validate/artifacts/`.
2. **Add receipts for older main commits** while bisecting. Each bisect probe is a real full validate at a historical main commit, recorded in the ledger as that commit's status. Prefer first-parent main commits that have no full record.
3. Propose and land the speed fixes, starting with caching the pinned-root workspace build across validates of nearby commits (today every run compiles from a fresh checkout).

## Rules
- **Never compete with landings.** At most 2 full validates on the host at once, counting the lander's and kvm's. Launch only when load1 < 0.8 × nproc. Run through `ci-hub validate-run` (herdr-run ci-hub-cli) so validate-lock and the load gate see you. Prefer quiet hours.
- **Conservative landing protocol (owner, session-only):** a Hermit change lands only with a full validate at the exact SHA that lands. museland lands Hermit; you hand back.
- Every timeout you report names wall or CPU (owner rule). CPU caps target about 50% headroom; wall backstops are generous (≥3× CPU).
- Do not edit skills or AGENTS.md.
