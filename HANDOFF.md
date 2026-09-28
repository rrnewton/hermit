# PR529 + PR538 fixed-base diagnostic handoff

This is local integration evidence only. Nothing was pushed or merged and no
remote branch was changed.

## Source

- Slot: `/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic`
- Hermit HEAD: `5c0bae832d515ce2b8b9cfcf2d7b97f010d641c6`
- Hermit tree: `b32b3a432c5f3346b0e02c91a0cf983170e49618`
- Hermit product-source parent: `9d4bb692ddfe02241e6341a2faeb83782215e1a5`
- Expected tracked status: only `Cargo.lock` modified for the local path overlay
- Combined Reverie checkout:
  `/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-reverie`
- Combined Reverie merge: `d0decf28738521b9ae0a33c4b930c5f3f3d43d27`
- Combined tree: `209cf49e90bb83787865e300fb3c56f3add1813d`
- Ordered parents: PR529 `c632c111619cb47922a72235b3e1130b91355603`,
  then approved PR538 repair `90ad5b98fa897f03e817d74b1fa66e68f1b758fb`

The exact result audit, including every nonpass's first divergence or timeout,
source hashes, current-main selection check, and socket message-state overlap,
is `integration-evidence/combined-27-cell-audit.md`.

## Completed combined run

- Result directory:
  `integration-evidence/brs-pr529-pr538-90ad-r2-canonical-1900877-1789013353477743636`
- Canonical record:
  `/home/newton/work/dev-hermit/ignored/validate/runs/validate-kvm-pr529-pr538-5c0bae-20260910T040836Z.json`
- Canonical log:
  `/home/newton/work/dev-hermit/ignored/validate/validate-kvm-pr529-pr538-5c0bae-20260910T040836Z.log`
- Result: infrastructure complete; service/bench exit 0; 634.187 seconds
- Shape: 27/27 identities, 27 invocations, 27 rows, attempt 1 only, zero
  retries, no relaxations
- Outcomes: 11 PASS, 11 deterministic FAIL, three CPU timeouts, two wall
  timeouts
- Qgroup: 2,409,185,280 referenced and 670,916,608 exclusive bytes out of
  12,884,901,888 bytes
- All validation-lock slots were free after release.

The preceding failed infrastructure attempt is intentionally preserved as
unit `validate-kvm-pr529-pr538-5c0bae-20260910T040053Z.service`, with its
canonical record/log and result directory
`integration-evidence/brs-pr529-pr538-90ad-1168374-1789012321389076699`.
It executed zero cells; locale-dependent ordering, not membership, caused the
27-cell identity check to stop before builds/runs. The fixed launcher exports
`LC_ALL=C` and separately checks exact set and order.

## Causal-comparison arms prepared, not launched

See `integration-evidence/comparison-arms/README.md`. The PR529-only and
vectored-only arms use the identical Hermit 5c0bae source, exact C-ordered
27-cell set, glibc 2.42 image, jobs=1, one attempt, bounds, result schema, and
source/mutation guards. Each has a fresh empty 12 GiB qgroup. No canonical run
record or log exists yet; the wrappers create those atomically at launch. Do
not launch while another KVM test owns validation capacity.

## Important interpretation

The intended earlier PR529-only diagnostic ran 0/27 cells because its canonical
run record was absent. The older run13 source evidence is Hermit 9d4bb692 plus
pre-PR529 Reverie 8c8c0a57, with two wall-timeout attempts per selected cell.
It is not PR529-only evidence. Therefore the completed combined result cannot
yet isolate PR538's causal effect; use the two prepared arms.

The combined exact ignored fixture did prove all ten vectored descriptor
snapshots, including eventfd, before failing only at strict verify-log
nondeterminism. Its retained logs and updated handoff are in the sibling
`integration-pr529-pr538-hermit` slot.

Do not remove this slot or any bounded result directory until the handoff has
been consumed.
