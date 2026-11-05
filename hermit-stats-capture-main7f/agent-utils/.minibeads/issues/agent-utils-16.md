---
title: 'old-checkout-cleanup: land and remove local work'
status: in_progress
priority: 1
issue_type: chore
assignee: gpt-6
labels:
- repository
- cleanup
depends_on:
  agent-utils-1: parent-child
created_at: 2026-09-25T03:55:15.315366091+00:00
updated_at: 2026-10-02T21:37:45.408829213+00:00
claimed_at: 2026-09-25T04:17:03.192733505+00:00
claimed_until: 2026-09-27T04:17:03.192544933+00:00
---

# Description

Preserve and land every useful local commit and uncommitted change from the old checkout, reconcile it with current main, validate it, push a linear history, then leave the old checkout clean and remove temporary worktrees and branches.

Also audit every registered agent-created worktree and branch visible from the old checkout. Remove worktrees and delete branches only after proving their useful work is in origin/main or shepherding unique intended work through validation and landing; preserve ambiguous human-owned branches, prune stale missing-directory registrations, and retain the durable recovery bundle until the complete cleanup is verified.

# Acceptance Criteria

All useful old-checkout and registered agent-worktree work is landed or proven superseded; origin/main contains it; the old checkout is clean; merged agent branches and worktrees plus stale registrations are removed; ambiguous human-owned branches are preserved; no temporary reconciliation branch/worktree remains; and the durable recovery bundle is retained through final verification.

# Notes

[gpt-5] Read-only cleanup audit on 2026-09-29, against current `origin/main`:

- 13 worktrees are registered. Four are clean and point to commits already in main. Nine are not
  ancestors of main; seven are clean and two contain modifications, so none of those nine is safe
  to remove merely as stale registration.
- The nine unmerged worktrees reduce to five patch identities. The normal reply-burst patch is
  already in main as `30d7e315`; the exact-boundary retry, an alternate reply-breaker patch, and
  the two-commit chat-bridge stack are not patch-equivalent to anything in main.
- The old checkout's sole untracked issue comment is superseded: current main tracks the same
  content plus a later acceptance record. Its branch is 96 commits beyond its configured local
  origin, however, so that checkout still needs a deliberate history reconciliation before it can
  be removed.

No worktree, branch, recovery reference or bundle was deleted in this audit. The remaining action
is destructive and needs an owner decision about the unmerged stacks and the two dirty negative-
control/review worktrees.

[gpt-5] Follow-up reconciliation on 2026-09-29:

- The exact committed-boundary retry is now in `origin/main` as `d458192c`; both retained source
  commits are patch-equivalent to it. The normal reply-burst fix and its consolidated follow-up are
  likewise patch-equivalent to landed commits. Older alternate reply-breaker variants are
  superseded by that consolidated implementation.
- The old `main` checkout was clean and strictly behind `origin/main`; it was fast-forwarded to
  `797f57d0` and remains clean. No local work was discarded.
- New chat/inbox work appeared while this audit was running. Its active worktrees, the two dirty
  review/negative-control worktrees, every branch, and every recovery reference remain untouched.

Useful historical work is therefore reconciled without disturbing concurrent work. Final branch
and worktree removal still awaits an owner decision about the dirty and currently active targets;
the durable recovery bundle remains retained.

[gpt-5] Rescue and cleanup pass on 2026-10-02:

- The mistakenly allocated AgentCloud workspace was audited and removed. Public `main` was
  rescued by a plain fast-forward through `5aba7e81`; no rebase, force push, or PR was used.
- A self-contained `0600` recovery bundle outside the old clone preserves 462 audited refs,
  including all 71 recovery refs and every tip found unreachable from the observed remotes. Its
  SHA-256 is `250c81ef10fe58b3ea285c1f197ae0ea4d61a832833a81b820e9dd523feaec3c`.
  Bundle verification, exact ref-manifest comparison, a full restore, strict fsck, and recovery-
  only object checks all passed.
- Twelve clean, inactive worktrees were removed through `git worktree remove` after process,
  dirtiness, integration, and recovery checks. The Slack deployment worktree was also removed
  after its exact commit reached GitHub. Their branches and recovery refs were preserved.
- Four old-clone worktrees remain intentionally: the clean primary checkout is still used by a
  live shell; one detached negative-control tree has 160 uncommitted insertions; one checkout is
  the cwd of an adb process; and one chat-bridge checkout has active Claude processes and unique
  unlanded work. The issue stays open rather than deleting or misclassifying those targets.
