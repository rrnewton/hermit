# pr-landing-planner adversarial review — 2026-08-05

An independent reviewer attacked safety decisions, fixture and evidence schemas,
priority hooks, randomized graphs, public documentation, and both distribution
artifacts.

This review record entered the repository with implementation commit
`5ef91c55036227b5dc2997ef069784f337b4cc5e`.

## Findings resolved

1. Required or changes-requested review could still produce `land-now`. Required,
   negative, and unknown review states now hold fail closed.
2. Clean validation was bound to the head only. Evidence now carries and checks
   the exact fetched head and base SHA; missing or stale base evidence requests
   revalidation.
3. Ordering cycles degraded into singleton groups that could land. Every cycle
   member and affected downstream dependency is now held with a controlled
   diagnostic.
4. Batch construction ignored ordering edges. Parallel batches now contain
   dependency roots only.
5. Fixture and context readers accepted duplicate or nonpositive PR identities,
   invalid/self endpoints, unknown relations, empty identities, and negative or
   overflowing numeric fields. Both readers now enforce the same signed-64-bit,
   graph, and identity domains.
6. The priority hook silently defaulted after launch/exit/timeout/output errors;
   accepted non-ASCII and arbitrary-size integers; and could traceback when a
   label regex had no capture. Hook configuration and output now fail with the
   same controlled error in both implementations.
7. Public priority documentation exposed unrelated integration names. The
   public surface is the neutral `command` hook and package-local terminology.

## Evidence

- Focused Python planner suite: 87 passed.
- Native suite: 29 passed; Clippy and rustdoc warnings were denied, including
  missing public documentation.
- Strict type checking passed for the planner and differential harness.
- Differential: 150/150 checks with 100 randomized fixtures and seed `731942`.
- Both isolated wheel and registry-package checks passed.

Reproduce the extended differential with:

```sh
python3 cross/differential.py --tool pr-landing-planner --random 100 --seed 731942
```
