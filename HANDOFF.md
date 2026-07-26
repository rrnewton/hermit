# LiteInst Guest Trait Handoff

## State

Task: impl-liteinst-guest-trait (closed in tg)
Repository: Hermit
Slot: /home/newton/work/dev-hermit/worktrees/slot128
Branch: impl-liteinst-guest-trait-slot128
Base/uncommitted HEAD: cdbc55a6063ce29250bf94b07c16eee798d2c11a
PR: none
State: intentionally dirty and uncommitted. Do not reset, clean, or discard it.
Paired Reverie slot: /home/newton/work/dev-hermit/worktrees_reverie/slot111

## What Works

The literal Hermit CLI reaches the real generic path:
Hermit -> LiteinstBackend -> detcore-liteinst.so -> Detcore -> LiteinstGuest.
The old compatibility event-stream adapter was removed. The common container,
strict configuration, output capture, summary, and verify paths now dispatch
Backend::Liteinst directly.

Validated B2 scope: dynamically linked, single-threaded, single-process x86-64
guests. This exact command exits 0 and prints hello:
timeout 30s ./target/debug/hermit run --backend liteinst --strict -- echo hello

## Validation

- cargo test -p hermit --lib --bin hermit: 51/51 library and 63/63 binary.
- cargo test -p hermit --test cli --test liteinst_advanced: 47/47 and 3/3.
- L2 micro-suite: true, echo, and cat pass strict verify.
- Thread clone and fork fail-closed tests: bounded exit 1, no hang or SIGSYS.
- cargo clippy -p hermit -p detcore-liteinst --all-targets -- -D warnings: pass.
- cargo fmt --all -- --check and git diff --check: pass.

If Cargo reports Transport endpoint is not connected, use:
with-proxy env PATH=/home/newton/.cargo/bin:/usr/bin:/bin cargo ...

Build the executable and DSO separately. A --bin filter can leave a stale DSO:
cargo build -p hermit --bin hermit
cargo build -p detcore-liteinst --lib

## Dependencies

Hermit manifests temporarily use local paths into Reverie slot111. The root
manifest patches the Reverie git source to that slot. Replace these paths with
fetchable revisions before PR handoff. Parent gitlinks are unchanged.
RPC PR #98 is landed: https://github.com/rrnewton/reverie/pull/98
Preload PR #100 is open: https://github.com/rrnewton/reverie/pull/100
Reverie liteinst2 points at scratch/liteinst2-clean-separation.

## Known Gaps

- Thread clone, fork, and vfork return EOPNOTSUPP.
- Exec is unsupported.
- RCB timers/read-clock and CPUID/RDTSC interception are incomplete.
- Verify currently supplies /dev/null as stdin.
- Full application signal-disposition multiplexing is incomplete.

## Successor Steps

1. Read the paired Reverie HANDOFF.md and review both diffs.
2. Preserve both dirty slots and attribute any new changes before editing.
3. Make lower-level Reverie/liteinst2 dependencies fetchable and land them first.
4. Replace Hermit local dependencies with exact reachable revisions.
5. Rebuild the DSO explicitly and rerun every validation command above.
6. Only then create coherent commits/PRs and provide exact handoff SHAs.
