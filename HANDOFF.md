# HANDOFF — hermit-det3 (opus-5) — 2026-08-06 teardown

Slot: `worktrees/det3/hermit` (clean, detached at `4c70658e785834737cbe1524f77330c781a6f5ea`).
Nothing uncommitted. Nothing unpushed. Every SHA below was verified at the remote with
`ls-remote`, not inferred from a push exit code.

## Hermit branches (all pushed + remote-verified)

| branch | SHA | PR | state |
| --- | --- | --- | --- |
| `fix/parity-fixtures-emit-observed-value` | `492e78acd277…` (mine; branch now `e1bb1cac2`, another agent extended it — my commit verified still an ancestor) | #1719 | WIP, no receipt |
| `det/rusage-cpu-from-virtual-time-stack2-1` | `bff5a3a01314ae478dcbbd78459af82fde86b984` | #1688 | WIP, validate was QUEUED |
| `fix/determinize-filecontents-inode-stack1-1` | `a910f8738f0ce7ae25456aefc8f42d98d65e08c9` | #1674 | **SUPERSEDED by #1669 — close it** |
| `landing/ci-validate-integrity` | `6c5ce83c21be535c3d35e782fce180f14aba7ca4` | #1676 | **SUPERSEDED by #1675 — close it** |
| `mutation-audit-fixtures` | `2968c4e4fb64` | — | local-only scaffold, never pushed, safe to delete |

## Parent commits (pushed to rescue refs — parent main is UNSAFE, see below)

- `6ad23da660c3c888c56808ae2216b78e32960e83` → `rescue/det3-file-io-residue`
- `708fd9ab37aec3d69a605375513598e97fdbb7ac` → `rescue/det3-orphan-triage-real`
- `1823e2cfec20bcfbef501d1a4c291ce785e73bf2` → `rescue/det3-parity-c-evidence`
- 45 orphaned commits → `rescue/orphan-<short-sha>` (45 refs, counted at the remote)

⚠ `rescue/det3-orphan-triage` holds **another agent's** commit `5680db28` under my name —
mislabelled, not lost. I left it deliberately: extra reachability is protective right now.

## NEXT STEP, in priority order

1. **Do not run `git gc`/`prune`/`reflog expire`/`repack` on the parent.** Orphans are only
   safe because they are pushed; local reachability is still broken.
2. `backend_parity_contract_fixture_3` — **NOT STARTED.** The existing
   `sched_getaffinity_identity.c` only *prints* the mask; the task needs the guest to
   **branch** on it (derive a worker count and act), plus all three legs incl. non-vacuity,
   and land it `ci=true` with backends named.
3. `enable-proven-passing-parity-c-cells` — measurement done (**83 PASS / 2 hang** of 85;
   the rescued audit's "0 of 83" is a harness bug: it compiled fixtures without
   `-D_GNU_SOURCE`). **Not flipped to `ci=true`**, deliberately: one run per cell on a
   contended box, and I observed my own PASS cells hanging on re-check. Re-run with N=3–5
   and enable only cells green every time. Then regenerate `ci/expected-e2e-plan.json`
   (ratchet is live) and give every still-off cell a reason string.
4. DBI regression **I caused**: `libreverie_dbi_client.so` is no longer built into
   `target/release`; copying the staged one back does not fix it (DynamoRIO: "unable to
   process imports"). Try `cargo clean -p detcore-dbi` then rebuild with
   `--features third-party-backends`.

## Gates / environment (each of these cost real time today)

- Build/link: `PKG_CONFIG_PATH` + `LIBRARY_PATH` = `ignored/lu-parity/usr/lib64`.
  Runtime: `LD_LIBRARY_PATH=/home/newton/fbsource/.../libunwind/lib` (lu-parity ships only
  the static `.a`).
- `HERMIT_INSTALL_DIR=<checkout>/target/install_pkg` is **required for sabre** — undocumented.
- A guest binary **must not live in `/tmp`** (hermit gives the guest a private `/tmp`; every
  backend then fails in a way that looks like a backend bug). This produced a wrong finding
  of mine before I caught it.
- Parent commits need `HERMIT_PIN_DRIFT_OVERRIDE=1` while another agent's submodule state is
  in flight; committing *through* herdr-run instead lets the pin lint actually run and pass.
- **Never bind a post-commit action to a bare `git rev-parse HEAD` in the parent.** I did,
  and published a stranger's commit under my branch name. Bind by unique subject + a content
  check, or commit on a per-agent branch.

## Backends at the tip (measured, hello-world / L1 only)

ptrace ✅ · e9patch ✅ · **sabre ✅ (newly unblocked)** · **liteinst ✅ (newly unblocked)** ·
dbi ❌ (regressed by me) · kvm ❌ hangs, no output.
