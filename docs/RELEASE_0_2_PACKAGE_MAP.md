# Hermit 0.2 package map

This is the package-name and publication-surface map for the Hermit 0.2
release. It was audited from every `Cargo.toml` in this repository on
2026-08-03. Virtual workspace manifests are not packages. The table includes
the root workspace, nested build workspaces, and the standalone CI utility
workspace.

`Public` below means Cargo permits publishing the package. `Private` means the
manifest has `publish = false`; private packages are not subject to the 0.2.0
release-version floor. A name marked `PENDING-OWNER-DECISION` must not be
renamed or published until the owner selects it.

## Hermit release packages

| Manifest | Current package | Version | Intended published name | Surface | Status |
|---|---|---:|---|---|---|
| `hermit-cli/Cargo.toml` | `hermit` | 0.2.0 | `hermit-run` | Public | Rename approved and reserved, but not applied in this change |
| `detcore/Cargo.toml` | `detcore` | 0.2.0 | `PENDING-OWNER-DECISION` | Public | Current name is owned by another publisher |
| `detcore-model/Cargo.toml` | `detcore-model` | 0.2.0 | `detcore-model` | Public | Reserved by `rrnewton` as a 0.0.1 placeholder |
| `detcore-dbi/Cargo.toml` | `detcore-dbi` | 0.2.0 | `detcore-dbt` (`PENDING-OWNER-DECISION`) | Public | DBI-to-DBT rename is deliberately not applied |
| `detcore-sabre/Cargo.toml` | `detcore-sabre` | 0.2.0 | `detcore-sabre` | Public | Name was unclaimed at the 2026-08-03 audit; real publish would claim it |
| `common/digest/Cargo.toml` | `digest` | 0.2.0 | `PENDING-OWNER-DECISION` | Public | Current name belongs to RustCrypto |
| `common/edit-distance/Cargo.toml` | `edit-distance` | 0.2.0 | `PENDING-OWNER-DECISION` | Public | Current name is owned by another publisher |
| `hermit-resources/Cargo.toml` | `hermit-resources` | 0.2.0 | `hermit-resources` | Public | Reserved by `rrnewton` as a 0.0.1 placeholder |
| `hermit-verify/Cargo.toml` | `hermit-verify` | 0.2.0 | `hermit-verify` | Public | Reserved by `rrnewton` as a 0.0.1 placeholder |

These nine packages are the Hermit repository's public 0.2 release surface.
All are at the 0.2.0 floor and carry a crates.io description. The name map is
not permission to publish: dependency publication order and the unresolved
names above still gate a real release.

## Private and auxiliary packages

| Manifest | Current package | Version | Intended published name | Surface | Reason |
|---|---|---:|---|---|---|
| `common/test-allocator/Cargo.toml` | `test-allocator` | 0.0.0 | none | Private | Test-only allocator |
| `detcore/tests/testutils/Cargo.toml` | `detcore-testutils` | 0.0.0 | none | Private | Detcore integration-test helpers |
| `tests/Cargo.toml` | `hermetic_infra_hermit_tests` | 0.0.0 | none | Private | Guest programs for the test corpus |
| `flaky-tests/Cargo.toml` | `hermetic_infra_hermit_flaky-tests` | 0.0.0 | none | Private | Intentionally racy guest programs |
| `hermit-install/Cargo.toml` | `hermit-install` | 0.0.0 | none | Private | Internal installation/build helper |
| `ci/manifest-plan/Cargo.toml` | `hermit-manifest-plan` | 0.1.0 | none | Private | CI manifest planner |
| `liteinst-runtime-build/Cargo.toml` | `hermit-liteinst-runtime-build` | 0.0.0 | none | Private | Nested release-artifact builder |
| `liteinst-runtime-build/runtime/Cargo.toml` | `hermit-liteinst-runtime-artifact` | 0.0.0 | none | Private | Nested runtime artifact |
| `tests/reproducible-builds/build-time-0.1.3/Cargo.toml` | `hermit-repro-build-time` | 0.1.0 | none | Private | Reproducible-build fixture |
| `agent-utils/rs/safe-ci-dag-runner/Cargo.toml` | `safe-ci-dag-runner` | 0.2.0 | `safe-ci-dag-runner` | Public, auxiliary | Separate CI utility workspace; not in the Hermit product dependency graph |

The former `detcore-liteinst/Cargo.toml` was a source-less manifest outside all
workspaces, not a buildable crate. It is removed rather than versioned or
published.

## Cross-repository release dependencies

Hermit currently pins these Reverie packages from Git while also declaring a
0.2.0 registry requirement. `cargo publish` removes the Git source from the
packaged manifest, so matching real 0.2.x versions must exist on crates.io
before the dependent Hermit package can pass a faithful publish dry-run.

| Current dependency name | Intended registry name | Status |
|---|---|---|
| `reverie-core` | `reverie-core` | Source rename landed; registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-syscalls` | `reverie-syscalls` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-ptrace` | `reverie-ptrace` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-kvm` | `reverie-kvm` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-liteinst` | `reverie-liteinst` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-rpc-transport` | `reverie-rpc-transport` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-memory` | `reverie-memory` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-sabre` | `reverie-sabre` | Registry currently has an `rrnewton` 0.0.1 placeholder |
| `reverie-dbi` | `reverie-dbt` (`PENDING-OWNER-DECISION`) | DBI-to-DBT rename is deliberately not applied |

## Current release flake gate

The current Hermit release head pins Reverie
`d973a85b328610c14c41c39fa57495b9f77c3c90`, which does not contain the
`PTRACE_GETEVENTMSG`/`ESRCH` spin fix from Reverie PR #355 (`820b2b64`). A
matched-load multisect measured 52 hangs in 320 executions (16.3%) at this
exact Reverie pin; affected neighboring pins measured 18.4-23.4%. The probe
used concurrent, interleaved waves under elevated host load and classified any
mixed result as **flaky**. Its method and raw data are recorded in the parent
workspace under `experiments/multisect_detcore_misc_20260803/`.

The post-#355 residual observation of approximately one passive-block hang in
2,760 executions is not evidence for this release head. It must not be quoted
as the current rate until Hermit pins a Reverie revision containing #355 and a
calibrated matched-load probe verifies that exact Hermit head. Until then, the
honest current status is a measured 16-23% `detcore_misc` flake, and the pin
bump plus exact-head verification remain release gates.

## Generated-manifest synchronization

Most product `Cargo.toml` files are generated by `autocargo` from fbsource Buck
targets. Before an fbsource import or a regeneration, mirror each public
package version/description, each `publish = false`, and each local dependency
requirement in the owning Buck target. A release is not complete while the
generated OSS manifests and their internal source disagree.
