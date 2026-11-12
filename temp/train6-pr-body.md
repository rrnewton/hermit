[hermit2, musetrain, tbh-1005, devbig014, role=impl]

## Plain Language Summary and Project Impact

Train 6 (sleep/time) of the VladimirMakaev PR drain: the sleep-registration stack combined into one branch on current main. The top branch (4938) contains 4934's rebased content, so this train picks 4938's commits only.

Constituents (pick-time heads, 2026-11-01):

- https://github.com/rrnewton/hermit/pull/4938 @ e83ed19c2f103ae514c5a822da998da59fe53e3a
- https://github.com/rrnewton/hermit/pull/4934 @ 8072280e0c8355e37df46f28c914e12038bab126 (content included via 4938's stack)

Combined-head merge decisions: the `TakeSleepInterruption` RPC handler was added beside main's newer handlers (additive merge); the global-RPC fingerprint list keeps main's revision 21, adds this stack's as revision 22, and appends the combined-head revision 23 with the digest the repository's own fingerprint test prescribes (`GLOBAL_RPC_REVISION` set to 23). This settles Train 0 open item (e)'s "RPC 22" at this head: 22 is this stack's fingerprint step. The validation DAG was regenerated.

## Determinism

Sleep registration at first pass and interruption at a timer's due time are deterministic scheduler decisions recorded in the schedule; no host wall time enters. Full validation at the exact combined head must pass before landing.

## Linux Semantics

Nanosleep interruption reports the time left from the timer's due time, matching Linux `nanosleep`/`restart_syscall` remaining-time behavior as reviewed in the source PRs.

## Validation

At combined head 97fa635d1b4aa0ecad97d2a83d4510cfb5c5f29f: cargo fmt check pass; cargo check for hermit-detcore, hermit-run, detcore-model pass; fingerprint test `the_global_rpc_revision_matches_its_definitions` pass; DAG regenerated and canonical. Full validate pending (queued behind Train 1 under the host validate cap).
