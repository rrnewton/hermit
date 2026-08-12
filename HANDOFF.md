# herdr-dev handoff — 2026-08-07, teardown

Slot: worktrees/herdr-dev/hermit (currently on fixture/shm-coherency-identity, clean)
Everything below is PUSHED and verified at the remote by ls-remote re-read.
Nothing is uncommitted. Nothing is unpushed.

## Hermit — 5 contract fixtures, all draft PRs, all ci=true and ratchet-verified

| PR | Branch | SHA | Surface |
|----|--------|-----|---------|
| #1699 | fixture/proc-sys-read-identity | 71a66612bf3c13fa9f2437148da2c53fd18cd67d | /proc and /sys reads |
| #1702 | fixture/startup-surface-identity | 1a674f2fca76924e24630bb0446dd2730b925b8e | env, auxv incl AT_RANDOM bytes, raw vDSO base, stack layout |
| #1717 | fixture/errno-path-identity | 878a7ead77c5cf5626646cf3ca6041ee52991736 | errno / error paths |
| #1723 | fixture/file-timestamp-identity | a88092465c4ed0a790fdca9cbeaca43ed5b10af8 | guest-created file timestamps |
| #1728 | fixture/shm-coherency-identity | 6c9495a880eecf0dc28cf7e3067913be0c34d360 | what each process OBSERVES through a shared mapping |

Each base = hermit main 4c70658e7. Each verified BOTH ways (can-fail + passes)
and each moved the verify-cell ratchet 77 -> 78 on its own branch, so none is
born ci=false. NONE MERGED — det2 lands serially.

GATE: all five append to the same three shared files (tests/e2e/manifests/
system-utils.toml, the inventory json, ci/expected-e2e-plan.json). Textual
conflicts only, never semantic — each appends a distinct block. Whichever lands
after the first needs a rebase, and a rebase invalidates its validate record.

## Hermit — WIP, pushed to avoid loss, NOT landable

stack1/dettid-detpid-split @ 1385f0a2e8477bef0d45d7df45ed9d6d933b2552
DOES NOT COMPILE — 50 E0308 errors, deliberate. It is the DetTid/DetPid newtype
split with scaffolding complete and ~50 semantic sites unresolved.
NEXT STEP: resolve the 50 sites. Reproduce the list with
  cargo check --workspace --message-format=short
plus PKG_CONFIG_PATH + LIBRARY_PATH under ignored/lu-parity/usr/lib64 and
RUNTIME LD_LIBRARY_PATH=/home/newton/fbsource/fbcode/third-party-buck/platform010/build/libunwind/lib
Without those the build dies in the unwind-sys build script and reports
"1 error" that has nothing to do with the change — a no-result that reads clean.
DO IT AS A BEHAVIOUR-PRESERVING PR FIRST: explicit named conversions at all 50
sites, zero runtime change, existing tests must pass unchanged. Deciding all 50
semantics inside the type-split PR mixes a mechanical refactor with 50 behaviour
changes in scheduler paths, which is unreviewable. lib.rs:1294 is the one real
finding (init_thread_state derives a pid from a tid) and deserves its own PR.
NO GREEN INTERMEDIATE STATE: 50 of 50 or the branch is red.

## Reverie

PR #389, stack-ptracer/liteinst-stats-off-ptrace-crate @ 097868594f530ff2e179d94cc6f816ce6ad8d1b5
base dd3c178. Moves LiteInst counters out of the ptrace crate. Step 1 of the
ptracer-removal sequence. Not merged.

## agent-utils — landed direct-to-main (that repo's policy)

d779d54 herdr-run · c83bcee tilde expansion · 78c79a8 cargo/retention/timeout
d6de4448 cwd mistargeting fix · 2450511 invocation classification doc

## Parent dev-hermit — landed, all verified at remote

027d7f0 ptracer-removal design · 66b50c9 ledger storage options
50664a4 Measured type · 88e99c2 Measured dead-code allow
aa78fa4 ci-hub/landing/staged-freshness.sh
1857318 / 7345eee agent-utils gitlink pins

## Open items I did not finish

- 39 of 59 paths in the shared parent index are STALE and would revert landed
  work. Detect with ci-hub/landing/staged-freshness.sh. Do NOT reset/checkout.
- The six live-owner leaf paths could not be integrated: the authoritative list
  was not findable. Whatever the six are they MUST be a subset of the 10 that
  pass freshness (all under compat-envelope/).
- patch_site_inventory_positive: backends are UNBUILT, not unavailable. Sources
  are all in-tree, cmake+ninja already installed. Build path is
  cargo build --features third-party-backends plus hermit-install. Only the KVM
  hang is a separate problem.
- green-time already exists locally (836c019, 7e568e3) with UNKNOWN handling; it
  needs LANDING, not writing, and is not wired as a ci-hub builtin yet.
