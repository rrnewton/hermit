# 4735 rebase brief (shepherd, prepared 2026-11-06)

PR: https://github.com/rrnewton/hermit/pull/4735 — Replay provenance + aarch64 LSE ADRP+LDR reader fix
Branch: drain/4735 @ 90bbe4baf480b5d1edbeb9584ee3a77abb1618ab (base fdbbe1245ea9 per de84 preflight; 4 mushep18 commits on 47a7f9abe2 + cherry-picked fix 40cf47bb)
Target main: 1f21fba3977bbd2e5671ceb79b38b467893049fd (or then-current tip at dispatch; re-fetch first)
Approvals: mushep18 muse @47a7f9ab, revastra2 codex @049ec300 (provenance-r4), revastra2 delta APPROVED-AT codex @90bbe4ba (reader-carry). Mechanical rebase carries approval; product change beyond union needs new delta.
Measured conflicts vs de84: detcore-model/src/config.rs, detcore/src/config.rs, detcore/src/tool_global.rs, hermit-cli/src/bin/hermit/run.rs + DAG pair. Notice vs 1f21 names config.rs + run.rs (queue's list may be partial — verify all).
RPC: live-compute on fresh main (982492df landed an RPC row since base): 4735's provenance revision number must be re-derived against fresh detcore/src/config.rs rows + RESERVED + ignored/drain-20261103/rpc-revisions.tsv (28=4789, 29=4884, 30=4728, 31=4867 known). Renumber-only = mechanics; re-measure digest.
DAG: main files + re-apply intended typed entries + regen + --check + manifest-plan tests; pins main+(head-base), never --allow-drop.
GOALPOST-MOVING REVIEW RULE: weakening assertions, widening tolerances, adding exemptions, skipping cases, relaxing comparators, renaming/relabeling failures as passes, or deleting checks instead of satisfying them are explicit review targets; lowering the bar does not clear it.
Targeted tests (x86 unit/planner honesty, no aarch64 execution claim): hermit-detcore aarch64_lse (was 77), record_serde/reserved_input/preemptions (was 151), hermit-run provenance (5) + aarch64_lse (1) - recount at new head, manifest-plan validation_dag, DAG --check, fmt, clippy touched pkgs. Prior full manifest-plan had 7 unrelated host runner-probe failures - compare against fresh main if seen.
Recovery ref before rebase. Recovery save inside clean temp/drain-4735 worktree only (slot-root save fails on temp embedded repos). Committer hermit-bot3 for new seal commits.
Flight: check merge-queue status + items/4735.json before push; never push in-flight. 4735 may stay held vs queued 4764 (config.rs) after resubmit - expected.
Push: herdr-run lease vs 90bbe4baf480, ls-remote read-back. One paced PR comment (<8KB). NOTE: spawner decides submit vs send-coordinator at dispatch time (last time coordinator submitted 4735; claudemq resubmit form names musetrain submit - follow dispatch instruction, not this file).
