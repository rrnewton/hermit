[hermit2, musetrain, tbh-1005, devbig014, role=impl]

## Plain Language Summary and Project Impact

Train 5 (binder/background) of the VladimirMakaev PR drain: the binder stack combined verbatim at its pick-time head into one branch on current main. The stack's top branch (4761) already contains the rebased content of 4742, 4751, 4847 and 4802, so this train picks 4761's 23 commits only — picking each PR separately would double-apply the stack.

Constituents (pick-time heads, 2026-11-01):

- https://github.com/rrnewton/hermit/pull/4761 @ 0b11743170f9efd5be4ed8780e9681307d7e5c3e
- https://github.com/rrnewton/hermit/pull/4751 @ 5337ebf676c9c8c2cd7937934054b70b4a481bb7 (content included via 4761's stack)
- https://github.com/rrnewton/hermit/pull/4742 @ c740cadb59d2c345de52fb046017273858db1092 (content included via 4761's stack)
- https://github.com/rrnewton/hermit/pull/4847 @ aa89b2215750fdc5cbfe1f6103d310cebec530e7 (content included via 4761's stack)
- https://github.com/rrnewton/hermit/pull/4802 @ 9478b9ba9b061731487b88911095402b7a66efac (content included via 4761's stack)

The validation DAG was regenerated at the combined head; no other new code.

## Determinism

Constituents verbatim; backgrounded-call rejoin points are recorded in the preemption record and replayed at those turns, which is the deterministic mechanism under review in the source PRs. Full validation at the exact combined head must pass before landing.

## Linux Semantics

No guest-visible behavior change beyond the constituents' own, each reviewed in its source PR.

## Validation

At combined head 7f5faf6083fcd1f777f4d3ddee19171a0fa6c3c4: cargo fmt check pass; cargo check for hermit-detcore, hermit-run, detcore-model pass; DAG regenerated and canonical. Full validate pending (queued behind Train 1 under the host validate cap).
