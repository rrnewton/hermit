# Prepared causal comparison arms (not launched)

Both arms hold Hermit at measurement commit
`5c0bae832d515ce2b8b9cfcf2d7b97f010d641c6`, whose product-source parent is
`9d4bb692ddfe02241e6341a2faeb83782215e1a5`. They use the same C-ordered
27-cell identity, pinned glibc 2.42 image, `/dev/kvm`, `--jobs 1`, one
invocation and one attempt per cell, 180-second per-invocation bound, and
5,400-second whole-run child deadline as the completed combined arm.

Neither arm has been launched and no run record or log has been created.

## PR529-only arm

- Reverie commit: `c632c111619cb47922a72235b3e1130b91355603`
- tree: `93961fe62f708118e3a8b1d40a207c93c055bfa9`
- parent: `cf60111cb2c3781d98c7af1c85c6f7f395db8d4b`
- source archive SHA-256:
  `d745e59626ff9bb49cc84c1afdf5fe6b24086a29841888e48ac43e770501186b`
- copied executor SHA-256:
  `6ab166ad8a3a7b3d991a8a8c4334d084dd65f324f296459ce4d74768422bad28`
- Cargo overlay SHA-256:
  `6778bfc6395071ea9770fe32f5631216456cfe2c3703ecf7184064647c2103bf`
- measurement launcher:
  `pr529-only/measure-pr529-only-cells.sh`
- bench service wrapper:
  `pr529-only/launch-pr529-only.py`
- fresh empty 12 GiB qgroup:
  `/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/brs-pr529-only-c632-3367082-1789015440618621238`

## Vectored-only arm

- Reverie commit: `90ad5b98fa897f03e817d74b1fa66e68f1b758fb`
- tree: `962f40b6e487e82feb56b0da113f772376cba389`
- parent: `3646ba2c662f65b97e94d39f60852b62610ca5a0`
- source archive SHA-256:
  `59148dde68881c03729aea74a9efecc9d8cccf87eeb160e7726152336af5f71e`
- copied executor SHA-256:
  `cd3173080a837d2f33e30f1f95e8589db87cafc605b580b4e46a1fb0de617f5f`
- Cargo overlay SHA-256:
  `cb4e679773aacc5c550e11710c3d66ab58412bd75ab6e7d8bd327753a2eb6afa`
- measurement launcher:
  `vectored-only/measure-vectored-only-cells.sh`
- bench service wrapper:
  `vectored-only/launch-vectored-only.py`
- fresh empty 12 GiB qgroup:
  `/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/brs-vectored-only-90ad-3372702-1789015450305808498`

## Prelaunch checks

- Both measurement scripts pass `bash -n` and `shellcheck -e SC2016`.
- Both Python wrappers execute `--help` successfully.
- `cargo tree --offline --locked --config <arm config> -p hermit -e normal`
  resolves every used Reverie crate to that arm's copied source. The only
  diagnostic is the known unused `reverie-util` patch warning.
- Each stored source tree has an empty recursive diff against a fresh extraction
  of its bound Git archive. Adding `UNEXPECTED-SOURCE-FILE` to either fresh
  extraction makes the recursive diff fail, as required.
- Both launchers hash-bind the completed combined arm's full ordered
  `affected-cells.tsv` (SHA-256
  `1a5a2779f13bfaf4a763d10bf287f434653c3c3d77938df03dfbee06235838bf`),
  including every selector. They derive and sort the historical identities
  under `LC_ALL=C`, test set equality, test exact order, and contain independent
  mutations for a changed selector and a same-set/different-order table. They
  do not reread the unbound historical `population.tsv` for selectors.
- Both launchers bind the ambient `.cargo/config.toml` at start and finish to
  SHA-256 `30cec349c9e5cfb9147d8026be90a0ebb58988ec399125e7152f976de7f21733`
  as well as binding their explicit overlay config.
- Both retain the one-attempt runner mutation, require 27 invocations/results,
  reject exit/result mismatches and any infrastructure/prerequisite/incomplete
  row, require a strict matched canonical BitwiseInfoV1 report with bitwise
  parity and positive numeric INFO counts for PASS, restore the runner source,
  and require final tracked status exactly ` M Cargo.lock`.
- The two scripts differ only in exact Reverie commit/tree/parent, source/archive
  and Cargo-overlay paths and hashes, result/binary prefixes, and descriptions.
- The prepared qgroups are empty (16,384 referenced/exclusive bytes each) and
  each has a 12,884,901,888-byte hard referenced-byte limit.

Final file hashes at preparation time:

| Arm | measurement script | bench wrapper |
| --- | --- | --- |
| PR529-only | `e6854c23b12b46801fcf9148aaf8e29ea6884079ec71ced7e4839d00f12c4180` | `ac9654f25e7821c6ef91a0bebcd857722ab9a1b36a3841fe39081994beb32bee` |
| vectored-only | `b36a6815ace81062037e70987b0908d9d0c334edd694a21e252ea7e65d61c299` | `bb26c0e6cc294bffe13ecee1dc711b42947c2baa68ef7021865a61fd3fa5feb5` |

Shared result-classification guard SHA-256: `46a63c82b8921045a5e91cc8da313d6c94ecca0952d318048d3074b0d959fb3e`.

At launch, choose a new unique unit name accepted by the matching wrapper. The
wrapper creates (and refuses to overwrite) the canonical
`ignored/validate/runs/<unit>.json`, reserves the matching canonical log, starts
a retained bounded user-systemd service, and invokes `ci-hub validate-lock run`
with `kind=bench`, `max=1`, wait/hold 600 seconds, and child deadline 5,400
seconds. Do not launch either arm while another agent is using KVM validation
capacity, and do not retry a product cell.
