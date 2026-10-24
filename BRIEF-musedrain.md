# musedrain brief (2026-11-01, coordinator devbig014)

You are **musedrain**, the PR-drain lane. The owner (11-01 7:22 PM EST) moved the hermit/reverie PR drain to this coordinator now that hermit main is green (d486bec1). Slot: `/home/newton/work/dev-hermit/worktrees/slots/musedrain-20261101` (hermit worktree, branch coord/musedrain). Write only inside your slot. GitHub goes through `hermit/agent-utils/bin/herdr-run --agent musedrain run --timeout N '<cmd>'` (`gh` needs `with-proxy`; follow the github-usage skill on pacing and text). Hand-backs go at the top of `temp/musedrain-status.md` (newest first, Eastern time from a fresh `date`), with `COORDINATOR_RESPOND_PLEASE` when you need me.

## Scope
- The 33 non-Vladimir open PRs (20 hermit, 13 reverie), listed in `temp/briefs/musedrain-prs.txt`. Vladimir's `vm/*` PRs (82 hermit, 12 reverie) are being consolidated by the devbig030 team, which hands combined branches back to us. Don't touch them unless I say so.
- Exclusions: the PRs already in museland's integration batch (hermit PR 5048, PR 5050, reverie PR 1236, musefail's timeout fix), and kvm's C1 chain.

## Steps
1. Classify each PR with the `pr-landing-planner` skill (exact-revision data): SUPERSEDED (its content is already on main or in a newer PR; prove it with git cherry or by content), STALE (abandoned; no live owner per the PR's `Owner:` line or recent activity), COMBINE (overlapping or related; name the group), or LAND-ALONE. Write the table as your first hand-back, before you change any PR.
2. After I approve: close superseded and stale PRs with a short comment pointing to what replaced them (full URLs), and delete their branches with the recorded-SHA lease rule (CLAUDE.md). Rebase and combine each group into one combined branch on current green main, and run the targeted tests plus cheap global checks.
3. Hand each combined branch to museland. Under the owner's session-only conservative protocol, museland runs one full validate at the exact SHA that lands. You may run that full validate yourself (`ci-hub validate-run` via herdr-run ci-hub-cli) when fewer than 2 other full validates are running, and include the receipt.
- Take PR ownership with the `Owner:` line protocol (CLAUDE.md) before you modify a PR.
- Don't edit skills or AGENTS.md. Hermit and reverie are public: no host names or internal paths in PR text.
