# Exact ignored fixture: PR529 + approved PR538 repair

This worktree is local-only. Nothing was pushed, published, or merged remotely.

- Path: `/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-hermit`
- Branch: `codex/integration-pr529-pr538-f8bc-hermit`
- Hermit HEAD: `6c90880ebebcfff80478b555c05f500d6e307716`
- Parent: `f8bc4909ffb84277ebd51817fc173b9aba1f13a2`
- Tree: `6302d87c16b60acc859124e6eb2f17410ff0a264`
- Reverie overlay merge: `d0decf28738521b9ae0a33c4b930c5f3f3d43d27`
  (tree `209cf49e90bb83787865e300fb3c56f3add1813d`)
- Ordered merge parents: PR529
  `c632c111619cb47922a72235b3e1130b91355603`, then approved PR538 repair
  `90ad5b98fa897f03e817d74b1fa66e68f1b758fb`

The exact ignored command was run once, with every spawned Hermit process routed
through `/home/newton/work/dev-hermit/bin/safehermit` via `HERMIT_BIN`:

```text
/usr/bin/time -p env HERMIT_BIN=/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-hermit/.integration-safehermit cargo test --offline -p hermit --test preadv2_pwritev2_pipe kvm_current_position_vectored_descriptor_matrix_requires_pr529_and_pr538 -- --ignored --exact --nocapture
```

Build succeeded. Test result: one test run, zero passed, one failed, four
filtered, 18.74 seconds. All ten required descriptor snapshots completed:
readv pipe/socket/eventfd; current-position preadv2 socket/eventfd; writev
pipe/socket/eventfd; and current-position pwritev2 socket/eventfd. The earlier
scalarized-eventfd `EINVAL`/guest-fault defect is gone.

The remaining failure is strict verify-log nondeterminism, not vectored syscall
semantics: the retained logs have 2,579 versus 2,572 messages and first differ
at record 336. Run 1 completes
`futex(0x1b51910,265,4,NULL,NULL,-1)=Ok(0)`; run 2 takes
`Futex wait running immediately because it will fizzle (0 != 4)`, then returns
`EAGAIN`, after which close/clone/thread ordering diverges.

Evidence:

- `integration-evidence/exact-ignored-first-attempt-run1.log`, SHA-256
  `8f6920f6261252679867746100f6ac5df78622adee1c4b5bcb82d0f91b444926`
- `integration-evidence/exact-ignored-first-attempt-run2.log`, SHA-256
  `b44c4e9be79a2d423a0a6516861ce1d0bb867d3343d04982a0fc6dfa3f60a9dd`
- `integration-evidence/exact-ignored-first-attempt-safehermit.log`, SHA-256
  `e2fe1e8b76a5128236c02743276a6bcdd55efc3446d8c8261576d9f0dcbf3f31`
- safehermit run ID `20260910T031609Z-3756082`, binary SHA-256
  `f549d63d19c8ca19b803201ad15791be6d564998623ed062ecb6c7fa169ffab1`,
  untruncated, exit 1

The previous handoff and the first integration against old PR538 head 3646ba2c
remain preserved under `integration-evidence/`. The fixed-base 27-cell evidence
and prepared causal comparison arms are in
`/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/HANDOFF.md`.

Do not remove this worktree until the handoff has been consumed.
