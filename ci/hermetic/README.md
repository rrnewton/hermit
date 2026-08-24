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
ci/hermetic/run-split-validate.sh              # fetch (network) then build+test (no network)
ci/hermetic/assert-no-network.sh               # the boundary check, with a negative control
```

## The network boundary — what is and is not claimed

This is the claim to read carefully, because it is easy to overstate and the
earlier version of this file did.

Validate runs as **two phases with a network boundary between them**:

| phase | where | network | what it does |
|---|---|---|---|
| **fetch** | the host | **yes** | `cargo fetch --locked` into a `CARGO_HOME`. Downloads only; produces no build output. |
| **offline** | the pinned root | **no** | build **and** test, against that cache and the pinned toolchain. |

The network window is deliberately a **pure download**. That matters because
`cargo fetch --locked` cannot introduce variance: every byte it writes is
checked against `Cargo.lock`, which pins exact versions **and content
checksums** for registry crates and exact revisions for git dependencies.
Nothing can enter the build from the network except bytes that already matched
a hash. A phase whose entire output is checksum-verified is a far smaller trust
surface than a build phase that merely happens to have network available.

So, precisely:

- **Claimed, and enforced:** the build and the tests cannot reach the network.
  `--network=none`, asserted from *inside* the container by
  `assert-no-network.sh` as the phase's first act, aborting before anything runs.
- **Claimed:** the compiler is pinned (the offline phase runs in the nix root)
  and the crates are pinned (`Cargo.lock` versions + checksums).
- **Not claimed:** that the fetch phase needs no upstreams. It needs
  `github.com` and `crates.io`. It simply cannot lie about what it got.

The assertion carries its own **negative control**, and this is the part that
makes it evidence rather than decoration: run
`assert-no-network.sh --expect-network` on the host and it must report a
reachable network. A probe that only ever runs where the answer is "no network"
cannot tell an isolated environment from a broken probe. Measured on a validate
host, the host run correctly refuses to certify isolation (exit 1) while the
same script in the pinned root reports no route, no DNS and no raw TCP (exit 0).

**The useful failure mode.** If the fetch phase did not populate the cache, the
offline phase fails immediately and loudly — `cargo metadata` exits 101 naming
the pinned `reverie` git dependency — instead of quietly reaching out. Measured:
unpopulated cache → exit 101; after fetch → `cargo metadata` exit 0 and a real
compile of that same git dependency in 8.78s with no network.

### Where the phase node sets come from

They are **not invented here.** `ci/portable-shards.json` already partitions the
DAG, and GitHub CI already runs that partition as separate jobs. The split
script reads the same keys with the same `jq` expressions as
`.github/workflows/ci-portable.yml`, and `ci/check-shard-coverage.sh` fails
closed if the map drifts from `ci/dag/portable.json`.

Two things worth knowing before touching this:

- **The partition is the shard map, not the `group` field.**
  `build.e2e_artifact` has group `build` but lives in the `integration` shard,
  so it is test-side; `setup.manifest_plan`, `setup.nextest`, `e2e.metadata` and
  `e2e.audit_compile_backend_parity_c` are not group `build` but are build-side.
  Partitioning on `group` misplaces five nodes.
- **GitHub's split is for wall clock, not for network.** Its shard jobs have
  full network and restore a cargo cache with `Swatinem/rust-cache`; the
  prebuilt tarball carries only binaries, not `target/debug/deps`, so the shards
  really do compile. GitHub enforces no network boundary anywhere. This path
  mirrors GitHub's node sets and their order, then adds a boundary GitHub does
  not have.

## Why nix and not just an OCI digest

An OCI digest pins the **artifact**. If the registry loses the blob, a validate
run from a month ago cannot be reproduced. A `flake.lock` pins the **inputs**, so
the image can be rebuilt from source at that lock even after third-party
upgrades. A receipt should carry both: the digest says what ran, the lock says
how to rebuild it.

## What a month-old rebuild of **the image** depends on — measured, not asserted

Scope note: this section is about rebuilding the **pinned root image** from
`flake.lock`. It says nothing about the crate dependency graph, which is pinned
separately by `Cargo.lock` and fetched in the fetch phase described above. The
two are different guarantees with different upstreams and it is a mistake to
read one as covering the other.

Even for the image the guarantee is **not** self-contained today, and a
reproducibility claim that quietly depends on an upstream staying up is worth
nothing.

`flake.lock` pins each input by revision **and `narHash`**, so content is fixed —
a moved tag or a force-push cannot silently change what you get. But rebuilding
still has to *fetch* those sources. A rebuild needs:

| dependency | what breaks without it | risk |
|---|---|---|
| `github.com/NixOS/nixpkgs` @ `b134951a4c9f` | cannot evaluate at all | low — commits are durable, but GitHub availability is assumed |
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

1. **Vendor the sources.** A pinned nixpkgs tarball is 44 MB, already fetched to
   `ignored/hermetic/vendor/` during this work
   (`b134951a4c9f3c995fd7be05f3243f8ecd65d798`, sha256
   `854a570860e89c1d649aa9f395f9a699e6d99ecd9829a7a0c1a27e5157b243ba`). The Rust
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

## Known gap: the DAG runner cannot box itself inside the container

The split script drives real DAG nodes through `ci/run-node.sh`, the same
entrypoint `scripts/validate.rs` and GitHub CI use. Inside the pinned root that
runner reaches its engine and then **fails closed**, for a reason that has
nothing to do with the network:

```
[safe-ci] ERROR: systemd --user scope is unavailable; refusing advisory-only containment.
<runner>: ERROR: cgroup boxing could not be established ...
```

The `<runner>` prefix is the DAG runner's own program name, not a fixed string:
it comes from `PROG` in the runner's CLI, so it tracks whatever the tool is
currently called. Quoting it literally here would rot the moment the tool is
renamed — which it was, mid-flight, while this branch was open. Grep for the
`cgroup boxing could not be established` half, which is stable.

That refusal is correct — resource boxing is the runner's primary purpose and it
declines to pretend. It is also the expected consequence of the stage-1 shape:
the **outer** `systemd-run` and cgroup policy were meant to provide boxing, with
the container supplying only the filesystem mechanism. Reconciling the two —
either delegating the existing scope's cgroup into the container, or running the
container inside that scope and passing `--allow-cgroup-failure` to the inner
runner — is stage-3 integration work and is **not** done here.

So what is proven today is the boundary and the toolchain, end to end:
`assert-no-network.sh` passes inside the container, `cargo metadata` and a real
compile of the pinned `reverie` git dependency succeed offline, and `cargo
fetch` inside the phase is refused. What is **not** yet proven is a full
39-node validate running to completion in the pinned root.

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
