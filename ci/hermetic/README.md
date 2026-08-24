# Hermetic validate — pinned root (stage 2, OPT-IN)

Nothing here runs by default. The ordinary validate path is unchanged; flipping
the default is stage 3 and the owner's call on the evidence this produces.

## What this is

A nix-built OCI image that pins the toolchain and every system executable a
manifest runs as a hermit guest, plus a runner that executes a command inside it.
The shape is stage 1's recommendation unchanged: the existing outer
`systemd-run`, `validate-lock` and cgroup policy stay, and this adds only the
filesystem mechanism — one privileged podman container pinned by digest,
`/dev/kvm` passed through, no runtime network, read-only source at `/src`,
separate writable output and target volumes. No second cgroup layer.

```
ci/hermetic/build-image.sh                     # build from the lock, load, record the digest
ci/hermetic/run-in-pinned-root.sh --src DIR --out DIR -- CMD...
```

## Why nix and not just an OCI digest

An OCI digest pins the **artifact**. If the registry loses the blob, a validate
run from a month ago cannot be reproduced. A `flake.lock` pins the **inputs**, so
the image can be rebuilt from source at that lock even after third-party
upgrades. A receipt should carry both: the digest says what ran, the lock says
how to rebuild it.

## What a month-old rebuild actually depends on — measured, not asserted

This is the part worth reading, because the guarantee is **not** self-contained
today and a reproducibility claim that quietly depends on an upstream staying up
is worth nothing.

`flake.lock` pins each input by revision **and `narHash`**, so content is fixed —
a moved tag or a force-push cannot silently change what you get. But rebuilding
still has to *fetch* those sources. A rebuild needs:

| dependency | what breaks without it | risk |
|---|---|---|
| `github.com/NixOS/nixpkgs` @ `56c02bc00adc` | cannot evaluate at all | low — commits are durable, but GitHub availability is assumed |
| `github.com/oxalica/rust-overlay` @ `ab450d47a3f9` | no Rust toolchain | low, same caveat |
| `static.rust-lang.org` | **no dated nightly** | **the real one — see below** |
| `cache.nixos.org` | still works, but builds from source | high cost, not correctness: minutes become hours |

**The dated nightly is the weak link.** `rust-overlay` does not vendor Rust; it
downloads the `nightly-2026-07-29` artifacts from `static.rust-lang.org` at build
time. Old nightlies are not guaranteed to be retained indefinitely. If that
artifact is pruned, this lock stops rebuilding even though every hash in it is
still correct — and it will fail loudly on a hash mismatch rather than quietly
substituting a different compiler, which is the right failure but still a
failure.

### What would make the guarantee real

Two options, neither adopted here because both are storage decisions the owner
should make rather than something to slip in:

1. **Vendor the sources.** A pinned nixpkgs tarball is ~44 MB. (The sha256
   recorded here previously was for the superseded `b134951a4c9f` revision and
   has been dropped rather than left to mislead; re-measure it against the
   current pin if this option is adopted.) The Rust
   artifacts would need the same treatment, and they are the ones that matter.
2. **Export the closure.** `nix copy --to file:///archive` writes the entire
   built closure — **0.63 GB** for this image — to a local store. That is
   self-contained by construction: no GitHub, no `static.rust-lang.org`, no
   `cache.nixos.org`. At 0.63 GB per pinned root this is cheap enough to keep
   one per landed toolchain bump.

Option 2 is the honest way to say "reproducible in a month". Option 1 alone still
leaves the Rust download in the path.

## Toolchain bumps and rollback

A bump is **one reviewed change**: edit `flake.nix`, run `build-image.sh`, and
commit `flake.nix`, `flake.lock` and `image.digest` together. The script writes
the previous digest to `image.digest.prev` automatically, so rollback is putting
that value back or passing `--digest` to the runner. Confirmed working during
this change: adding `/tmp` to the image moved the digest from
`sha256:22c0f945cb4c…` to `sha256:d1301b3ae1eb…` and preserved the prior one.

## Failure mode, deliberately chosen

`run-in-pinned-root.sh` **fails closed** if the pinned image is not present. It
does not fall back to a tag and it does not fall back to the host. A run that is
not in the pinned root must not be recorded as if it were — that would be a
receipt claiming hermeticity it did not have.

## Notes from building this

- **The proxy authorizes per identity, and a container is not an authorized
  client.** Measured on a validate host: `api.github.com` returns 200 from the host
  through `with-proxy` and fails from inside a container even with
  `--network=host`. This is why the image is built with nix **on the host** and
  consumed by podman offline, rather than running nix inside a container — which
  is what blocked stage 1.
- **A nix-built root is minimal and has no FHS scratch directories.** Without an
  explicit `/tmp` the backend-abstraction gate failed at `mktemp`, and it failed
  in its *negative control*, so it reported itself untrustworthy rather than
  passing vacuously. Good failure mode; the directory still has to exist.
- The image pins `rustc 1.99.0-nightly (26ae60a9e 2026-07-28)`, which matches the
  commit stage 1 observed on the host, so the pin reproduces the toolchain that
  was already in use rather than silently changing it.

## Pin freshness — the policy, and the check that is missing

**A pin must be current when it is set, and refreshed deliberately.** Pinning by
revision + `narHash` and being up to date are two different properties, and this
directory has already demonstrated that satisfying the first says nothing about
the second: the original `flake.lock` pinned `nixpkgs` at a **2024-12-30**
revision while `rust-overlay` was pinned at that day's tip. The stale half was
invisible because the Rust toolchain comes from the overlay, so nobody looking at
"is the toolchain right?" would ever look at nixpkgs. At that revision
`pkgs.rustc` was **1.77.2**, which cannot build this repository at all — 30 of
its crates are `edition = "2024"`, which requires 1.85 or newer. That silently
made "could we drop `rust-overlay` and use nixpkgs' own Rust?" look answered in
the negative when it was not.

Do **not** fix this by switching to a branch name. A branch is not reproducible.
Re-pin to a fresh revision as a reviewed change, exactly as a toolchain bump is.

### Proposed check (not built here)

Nothing detected the staleness. It sat 20 months out of date, landed on `main`,
and was found only because someone was reading the lock for an unrelated reason.
A check is cheap because the answer is already in the file: every node in
`flake.lock` carries `locked.lastModified` as a Unix timestamp.

- **What it does.** Read `ci/hermetic/flake.lock`, compute `now - lastModified`
  for **every** input, print each age, warn past ~90 days and fail past ~180.
- **Every input, not just the interesting one.** The failure here was one input
  current and one rotten; a check that only looked at "the input we care about"
  would have missed it precisely.
- **Where it lives.** `ci/hermetic/check-pin-age.rs`, a rust-script following the
  repository script convention and co-located with the data it audits per
  `docs/DIRECTORY_ACTIONS.md`, wired into the portable lane alongside the other
  lints.
- **Why it can run anywhere.** It needs **no network** — `lastModified` is
  already in the lock — so it works in the portable lane and inside a hermetic
  root, and it cannot itself become a fetch dependency.
- **Print the age even when green,** so a receipt records how old the pin was on
  the day it ran rather than only whether it crossed a threshold.
