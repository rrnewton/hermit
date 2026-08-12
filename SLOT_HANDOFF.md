# HANDOFF — slot `det2` (agent `hermit-det2`)

Written at quiesce for owner teardown, 2026-08-06.

## State: CLEAN. Nothing uncommitted, nothing unpushed.

Both slot children are clean and both branch tips are verified **at the remote by
`ls-remote` re-read**, not by a push exit code:

| child | branch | tip SHA | remote |
| --- | --- | --- | --- |
| `hermit` | `fixture/getcpu-identity-observed` | `1acefc75f375e811e0c5f437b100dda514921e03` | confirmed |
| `reverie` | `fix/sysinfo-zero-padding-on-conversion` | `23970b972871447059873ff0cc86800f76e5e571` | confirmed |

`git rev-list HEAD --not --remotes --count` = **0** for both. (`--all --not --remotes`
reports 1143/114, but that counts every local ref in a shared clone, not this agent's
work — do not read those as exposure.)

`worktrees/det2/liteinst2` does not exist; this slot only ever had two children.

## Last two tasks were verification-only — no code was written, by design

Both dispatches were research/verification. There is no in-progress edit to commit.

### `verify-the-five-closed-certification-gaps-independently` — tagged `implemented`, `in_progress`

Verdict: **3 of 5 CLOSED, 2 PARTIAL.** Full evidence is in the task notes.

- **Gap 2 CLOSED** — `getcpu-identity` flipped `ci=false`→`true` and is non-vacuous:
  native fails (`cpu=241`), hermit passes (`cpu=0`).
- **Gap 3 CLOSED, complete** — 425 `backends_disabled` sections, 1678 named reasons,
  zero empty.
- **Gap 4 CLOSED for DBI**, then extended (below).
- **Gap 1 PARTIAL** — producer gate correct; **zero of 20 scorecard CSVs carry a
  `mapped_sites` column**, so no artifact records the verdict.
- **Gap 5 PARTIAL** — 46 of 73 fixtures still emit exactly `<name> ok=N`. Includes
  `personality_domain` (`pers ok=5`), one of only two CI-enabled parity-c cells.

### `extend-backend-scoped-fixture-verification-beyond-dbi` — tagged `implemented`, `in_progress`

All six backends measured individually with a work count. Runs cached under
`ignored/g45/backends/` (gitignored, will not survive teardown — the numbers are in
the task note).

| backend | rc | work count | bucket |
| --- | --- | --- | --- |
| ptrace | 0 | 153 DETLOG lines, 164 syscall events | EXERCISED |
| dbi | 0 | `branches=52149 syscalls=74 rewritten=73` | EXERCISED |
| e9patch (inline-asm guest) | 0 | `candidate_sites=136; mapped_sites=136` | EXERCISED |
| e9patch (libc-only guest) | 0 | `candidate_sites=0; mapped_sites=0` | **NOT-EXERCISED** |
| sabre | 0 | `ptrace_fallback_sites=0 trusted_shared_object_sites=0 guest_rpc_observed=true` | AMBIGUOUS |
| liteinst | 1 | — | CANNOT RUN — preload handshake, fails on `/bin/true` too |
| kvm | 124 | — | CANNOT RUN — hangs, `/bin/true` at 90s, with and without `--strict` |

## Next steps, in priority order

1. **Regenerate the e9patch scorecard.** This is the only remaining part of Gap 1 and
   it is **unblocked** — `make install-deps` (54s) stages `e9tool` and `sabre`, and
   `e9tool` also already existed in-tree at
   `worktrees/250/hermit/target/install_pkg/rsrcs/e9tool`. Export `HERMIT_E9TOOL` and
   `HERMIT_SABRE_BINARY`. Expect many currently-L2 cells to flip to `not-exercised`.
2. **Fix `ci-hub/validate/sabre_reach.py:70`** — its regex is `patched_sites=(\d+)`,
   a counter the SaBRe path never emits (real fields: `ptrace_fallback_sites`,
   `trusted_shared_object_sites`). It is fail-closed (exit 1) so it cannot bless a
   vacuous cell, but it can never certify one as exercised either.
3. **Gap 5**: 46 fixtures need observed-value output; start with `personality_domain`
   because CI actually runs it.
4. **`ci-hub/validate/e9patch_reach.py` is UNTRACKED** in the parent and has no
   production caller (the live gate is the Rust one in `collect-e9patch-compat.rs`).
   Land it or delete it; do not leave a second implementation drifting.

## Gates / cautions

- **KVM and LiteInst cannot run any fixture on this box.** Do not schedule work that
  assumes they can without re-checking.
- **Check an artifact exists and is current before concluding a backend is broken.**
  I recorded sabre/e9patch as "unavailable" when the real cause was unstaged deps.
- **A search that did not complete is not a negative result.** I published
  "filesystem-wide search for e9tool found none" from a `find` that had been
  backgrounded on timeout; five copies existed. Corrected in the task notes.
- **The shared parent index held 106 foreign staged paths** through at least one full
  task cycle, with no owner claiming them. I touched none of them. Anything committed
  in the parent without explicit pathspecs will sweep them in.

## Parent-repo artifact

`ai_docs/reverie-pin-batch-bump-premise-refuted-20260806.md` is untracked in the shared
parent on purpose — it is published on branch
`ai-docs/reverie-pin-batch-premise-refuted` @ `e832750b6d3a9343bfc3ac1301860b08030d1028`,
committed from an isolated detached worktree so it never entered the occupied index.
