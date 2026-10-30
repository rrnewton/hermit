# musetrain brief (2026-11-01, coordinator devbig014)

You are **musetrain**, Train 0 of the PR drain. The owner moved the hermit/reverie PR drain to this coordinator on 11-01; devbig030 (coord5) handed over the Vladimir cluster. Slot: `/home/newton/work/dev-hermit/worktrees/slots/musetrain-20261101` (hermit worktree, branch coord/musetrain). Write only inside your slot. GitHub goes through `hermit/agent-utils/bin/herdr-run --agent musetrain run --timeout N '<cmd>'` (`gh` needs `with-proxy`; follow the github-usage skill on pacing). Hand-backs go at the top of `temp/musetrain-status.md` (newest first, Eastern time from a fresh `date`), with `COORDINATOR_RESPOND_PLEASE` when you need me.

## Scope: the 94 VladimirMakaev PRs (hermit 82, reverie 12; 16 drafts)
Hand-over facts from coord5: none is certified READY; none is confirmed already landed (85 of 94 heads were never fetched); 72 have NEEDS-FIX verdicts bound to 10-29 heads, and 65 of those heads have moved since; the BLOCKED count is unresolved (1 vs 30). Chains A–G plus reverie stacks are mapped in coord5's planner report, which coord5 is committing to dev-hermit (look under ai_docs/transient/ for vladimir-pr-planner; ask me if it hasn't arrived). Vladimir's agents are still committing (e.g. 4847, 4761, 4878), so heads keep moving.

## Train 0 (this task)
1. Fetch every Vladimir PR head (authorized; paced). Record head SHA, base, and whether its content is already on main (git cherry or patch-id; prove each claim).
2. Re-review at the CURRENT head with the code-review skill (adversarial and goalpost-moving checks). Re-derive each verdict: READY / NEEDS-FIX (name the fix) / BLOCKED (name the blocker) / SUPERSEDED / ALREADY-LANDED.
3. Reconcile chains A–G and the reverie stacks into landable trains, ordered by dependency, with each train's expected conflicts. Also settle coord5's open items: (b) the BLOCKED 1-vs-30 count; (e) RECORD 0x199 / RPC 22 and PMU-H status at pick time; (f) reverie 1146 disposition and the 1184 patch-id proof; (g) bot-PR overlap, including 4803, and 4557 vs 4892 (musedrain owns the bot PRs, so coordinate with it); (h) GitHub reverie main vs local pins. Escalate to me: (c) the core calls on hermit 4728/4741/4747 and reverie 1137/1141/1145/1149/1150/1157, and (d) the aarch64 validation lane.
4. First hand-back within about 24 h: the cluster map and the ordered train list. Change no PR before I approve the map.
- Take PR ownership with the `Owner:` line protocol (CLAUDE.md) before modifying a PR, and respect a live owner.
- Under the owner's session-only conservative protocol, every landing needs a full validate at the exact landed SHA; museland lands.
- At most 2 full validates on the host; launch only under the load gate. Don't edit skills or AGENTS.md. Hermit and reverie are public: no host names or internal paths in PR text.
