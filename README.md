# Salvage 2026-08-25 — hermit/ignored + hermit/scratch worktrees

Taken before reclaiming target/ from 28 registered git worktrees of the hermit
primary. Task: salvage-and-triage-nested-repos-then-delete.

cat3/<worktree>/
  BASELINE-HEAD.txt        the commit these changes apply ON TOP OF
  BASELINE-COMMIT.txt      sha, date, author, subject of that baseline
  STATUS.txt               git status --porcelain at salvage time
  staged-vs-HEAD.patch     index vs HEAD  (git apply --cached)
  worktree-vs-index.patch  worktree vs index
  worktree-vs-HEAD.patch   COMBINED — the one to reapply
  untracked/               verbatim copies of untracked files (in NO commit)
  index-added/             raw blobs of staged-added files (in NO commit)

Every worktree-vs-HEAD.patch was verified with
  git read-tree <BASELINE-HEAD> && git apply --cached --check --binary <patch>
and all 9 reapplied cleanly.

To restore one:
  git checkout $(cat cat3/<name>/BASELINE-HEAD.txt)
  git apply --binary cat3/<name>/worktree-vs-HEAD.patch
  cp -a cat3/<name>/untracked/. .        # if present
