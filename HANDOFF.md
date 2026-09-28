# Verify two ordinary runs — current continuation

Task: flip-verify-to-two-harness-managed-runs-with-retained-logs
Assigned Hermit slot: /home/newton/work/dev-hermit/worktrees/slots/dev-hermit-verify-two-runs-20260915
Branch: dev-hermit/verify-two-runs-20260915
Current committed HEAD: 7611a93d984d7c91dcd56f860ced26382afb598b
Current tree: 693eb77bc65a7f24f161b08ccbeedea8b344e43b
Original assigned base: 1468c4e207362247ef738e7a4b53bdd5097832c0
Tracked source/index clean; this HANDOFF is untracked authored evidence.
No push/PR/landing/full validation or actual Hermit guest invocation has occurred.

## Immediate next action

Independent Claude delta review is terminal: exit 0, APPROVE exact 4dbfb5fe,
154.522 seconds, six turns, same installed claude-opus-5[1m] model and dedicated
session 72a69bca-5208-41c0-a1d1-12e62d9e40c9. Artifacts:
 target/verify-two-runs/checkpoint-review/delta-{status.json,review.md,stdout.json}

Root then found an ancestor substitution gap that both reviews missed. It is
CONFIRMED before any product fix: the real public copy API returns Ok after a
mutating Write renames retained and replaces it with a symlink to the same
held leaf inode (nlink remains one). New regression test fails, cargo exit 101,
Dagrun exit 1. Exact source/diff/log/status in:
 target/verify-two-runs/ancestor-race-before/
Corrected and committed at 7611a93d984d7c91dcd56f860ced26382afb598b over 4dbfb:
held O_PATH directory chain, anchored openat/fstatat NOFOLLOW, same-leaf symlink
and ordinary directory substitution controls. All 12 reader tests, package
all-target Clippy -D warnings, fmt pass. Dagrun exit 0, 17.556 s, CPU 28.026108 s,
peak 1,209,352,192 bytes. Exact source/diff/checks under ancestor-race-after/.
No test is running; no new nextest/guest execution was needed for this reader fix.

Independent resumed Claude ancestor review is TERMINAL: exit 0, no timeout,
302.322 seconds, nine turns, actual claude-opus-5[1m] / canonical claude-opus-5,
Vertex. Dedicated session 72a69bca-5208-41c0-a1d1-12e62d9e40c9; PID 3900115
and tool handle 33330 are no longer live. APPROVE exact 7611a93d/tree693eb77b.
Artifacts target/verify-two-runs/checkpoint-review/ancestor-{review.md,status.json,
result-metadata.json,stdout.json,prompt.txt,frozen.json,diff}.
One nonblocking test nit remains: tighten the symlink mutation error assertion
from directory OR ancestor to directory AND changed identity, matching its sibling.
Do not change frozen source for this nit without root coordinating the new head.
Root's native complete checkpoint + ancestor review also found no blocking issue;
it is writing its exact certificate. Source approval is not cutover/runtime proof.

NEXT ROOT-ASSIGNED READ-ONLY SUBTASK COMPLETED: additive AU attempt schema plan.
 target/verify-two-runs/au-attempt-plan/PLAN.md (14,580 bytes)
Preserve CURRENT_SCHEMA=2 and all existing constructors/writers; explicitly opt
into classified schema3 with real complete attempt evidence, exact version
negotiation, separate outer and inner causes, and Python's existing refusal.
The plan lists exact AU sources/tests, seven saved content donors, Rust public
struct compatibility adaptations, later Hermit producer/consumer/pin chain,
raw duplicate keys, missing details, duplicate/missing attempts, cancellation,
legacy count preservation and scheduler terminal collision controls.
Saved AU bundle copied only into task-local bare saved.git for source inspection;
source-import.json has real exit and bundle SHA. No AU product source changed.
Root is preparing a separate AU lane. Keep Hermit frozen until root directs the
next implementation checkpoint; do not edit shared AU or another agent's slot.

After that continue selective S7 ordinary-run comparison/publication/consumer
recovery. Read target/verify-two-runs/publication-recovery-map.md. No production
cutover until every selected backend and consumer contract is implemented and
reviewed. Main still uses one internal --verify invocation per verify cell.

## Current coherent local checkpoints

7611a93d984d7c91dcd56f860ced26382afb598b — hold and authenticate every
retained log directory, including ordinary replacement with same leaf inode.

63deb02b8792cea9b91cd5a19e8b74bfd37ac85f — held retained-gzip identity readers.
c832145898b93a37f46d5c40a3cb3b8af108d20a — current Nextest held-cgroup owner
extracted to ci/manifest-plan/src/owned_cgroup.rs; old name generation and all
lifecycle/arbitration/cleanup controls preserved.
4dbfb5fe227fe8157e32e377b48bb3161bff509a — independent-review corrections:
single-member gzip+EOF, counterexample controls, unchecked-peer/copy-on-error
docs, canonical recheck paths, BorrowedFd enrollment API.

Independent Claude APPROVE exists for exact c8321458 only, original base1468,
tree c35112cfdc32fd4c935dcfb731db43da64c0d33a. Actual model from output:
claude-opus-5[1m], canonical claude-opus-5, Vertex. Its six nonblocking findings
were handled in4dbfb except harmless duplicate name validation, deliberately
retained. Certificate: checkpoint-review/review.md; real process status.json
exit0, 36turns, no permission denials. This was prerequisite source approval,
not whole-task or all-backend runtime approval. Delta APPROVE for 4dbfb is
also preserved; neither approval covers the new ancestor regression/fix.

Fresh4dbfb-source checks:10 reader tests,3 cgroup controls, rebuilt Nextest
wrapper --self-test (5.682s), package all-target Clippy -D warnings and format
all passed. Bounded4CPU/16GiB Dagrun21.658s, CPU29.252860s,
peak1,362,079,744bytes. Full command/log/status/source-digest evidence:
 target/verify-two-runs/checkpoint-delta/
Earlier source-bound tests, exact diffs and evidence remain under reader-checks/
and cgroup-extraction/. These are component checks, not official full receipts.

## Guardrails for subsequent recovery

Preserve decode_single_gzip_bounded and its matching-hash concatenation/trailing
byte controls. Replacing this file with S7 would reintroduce the reviewed gap.
Peer digest/size/count remain unchecked producer claims until bound to the
canonical comparison; readable gzip bytes alone never establish a match.
Copy Err may leave complete destination bytes; publish only success. Preserve
7611 held-directory anchoring and both same-leaf ancestor replacement controls;
restoring the old S7 pathname helpers would reintroduce the reproduced bug.

Use shared Hermit OwnedCpuCgroup, not old AU ManualCpuCgroup. Parent explicitly
approved this extraction and no AU API/pin solely to mirror S7. Pair accounting
must include both running and completed sides, final CPU before removal, and
permits held through reaping/empty/removal proof. Existing Nextest behavior and
all controls stay intact. Typed per-attempt Dagrun causes are a separate AU gap.

Keep all724 required verify identities:341 ptrace,243 KVM,112 SaBRe,28 LiteInst;
733 total required cells. Expected no-retry arithmetic724->1448 verify ordinary
executions,733->1457 total. None of those counts has been demonstrated by a new
runtime sample. Do not recover S7's SaBRe internal fallback or DBT refusal as a
finished route. Root/diagnose_rust_build is designing SaBRe host-supervisor log
transport; no Reverie write authorization belongs to this agent. DBT ordinary
capture/process-group compatibility and fixed workdirs still need real work.

Publication is one coupled transaction: compare both typed ordinary results,
verify/stage/sync exactly one gzip, durably publish the bound result row, THEN
remove raw inputs. All same-file writers share one publication lock, including
non-verify rows. Preserve restart, rollback, indeterminate and cleanup controls.

## Preserved sources and related work

Read target/verify-two-runs/prerequisite-map.md for all22 original prerequisites
by actual content. Current16 covered/current; six missing groups include one
old AU pin vehicle. Pure retention/cgroup recovery needs no new AU API.

Intact rescue: /home/newton/temp/dev-hermit/validate-s7-round3-9a0b6b78
HEAD9a0b6b782c955feb542102ba01aee88c2ebad240.
Bundle: /home/newton/work/dev-hermit/hermit/ignored/patchwork/s7-round3/hermit-s7-round3-final-9a0b6b78.bundle
SHA25620de1b6809a59151a29617994bcbdd14456f5c2d9fc3ccb477860cb0fde353b1.
Local Git import succeeded with --recurse-submodules=no. Initial recursive
attempt exited1; both statuses retained under target/verify-two-runs/.
Original rescue and bundle remain unchanged.

Separate schema6 chain, unreviewed/unpushed: parentc2944524e1b8f981774b22aaef8703f991752098
-> Hermitd9ddfef72e0bdc64f1a393034880a2d189571a7c
-> AUa640925d4ae400e8127ad2e1aa6c69c9ec2851c9.
AUrs/ unchanged; no old typed-attempt APIs. Reuse schema6 rather than duplicating
old S7 service changes. Do not edit sibling provenance slot.

Pin959ad5f180e8245e15f1588326d8a6a676264312 remains frozen and independently
approved in its separate mega-ci-passed-count-activation-20260915 slot. Do not
edit that slot again or transfer its compile/runtime identity here.

Writes only in this assigned slot/branch. No shared or skill edits. No remote
writes/full validation before root review and coordinated resource window.
All ad-hoc Hermit via /home/newton/work/dev-hermit/bin/safehermit.
All TG claims use dev-hermit; return notes to root or use tg-note-verified.
