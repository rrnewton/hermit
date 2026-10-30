[hermit2, musetrain, tbh-1005, devbig014, role=impl]

## Plain Language Summary and Project Impact

Train 1 of the VladimirMakaev PR drain: nine already-reviewed, merge-clean PRs combined verbatim (at their pick-time heads) into one branch on current main, so they land as one exact-head-validated unit instead of nine separate rebases. Each constituent keeps its own commits; the only new code is two combined-head drift fixes (below). Reverie companion: https://github.com/rrnewton/reverie/pull/1153 content rides on branch `train1-reverie-1153` (9a523fd8ab02) in the reverie repository.

Constituents (pick-time heads, 2026-11-01):

- https://github.com/rrnewton/hermit/pull/4948 @ 5b0daad08a8dd9c01665074bada802b98bef839a
- https://github.com/rrnewton/hermit/pull/4724 @ 9a090a68f854d9e30cb42249e13d83038a6cbdfc
- https://github.com/rrnewton/hermit/pull/4648 @ 62b0ef75bbc2483d4ec7c0a99b395a2cd376edff
- https://github.com/rrnewton/hermit/pull/4716 @ f571335bbfe9809f42580486997704316a468644
- https://github.com/rrnewton/hermit/pull/4888 @ c7435d5b3ba7b81e1e5a8de816f9bff1a76886c0
- https://github.com/rrnewton/hermit/pull/4754 @ 5781d0e0400d0790403e1b235020e9b22324d86d
- https://github.com/rrnewton/hermit/pull/4634 @ 80ae3e22faa6e30a146be7fb652e989842eed052 (stacks on 4626; only its 3 unique commits taken)
- https://github.com/rrnewton/hermit/pull/4626 @ 61629f200c7d52e98c82e1d3105fea754cca5ab0
- https://github.com/rrnewton/hermit/pull/4739 @ d9eada660e92ad14355badcbbd69ecf7ebf2babc

Combined-head fixes (31f07570f1): `AddedIdEvidence::capture` now passes `FaultWitness::Unproven` to the witness-taking readers 4888 introduced (fail-closed where no backend is known); `private_mqueue` uses main's `MountArg::get_target()` form. The validation DAG was regenerated after the 4739 picks.

## Determinism

No new nondeterminism: constituents are verbatim; the two fixes only narrow what is provable (Unproven witness refuses rather than assumes) and adapt to main's current mount API. Full validation runs at the exact combined head before landing.

## Linux Semantics

No guest-visible behavior change beyond the constituents' own, each reviewed in its source PR.

## Validation

At combined head 31f07570f1d68dfeabc1fba2c21e323d114b5034: `cargo fmt --all -- --check` pass; `cargo check` for hermit-detcore, hermit-run, detcore-model, hermit-manifest-plan pass; `generate-validation-dag --check` canonical; targeted unit tests pass (chosen_ids 24, access_trace 36). One full validate at this exact head is launched separately and must pass before landing.
