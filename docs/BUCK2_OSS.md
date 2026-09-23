# Reproducing the OSS Buck2 build

The OSS Buck2 project is generated from Hermit's authoritative root
`Cargo.toml` and tracked `Cargo.lock`. Reverie remains a separately pinned
source cell, but both cells resolve third-party Rust crates through Hermit's
single generated graph. Generated Rust `BUCK` files and vendored crate sources
remain ignored because they can be regenerated.

## Prerequisites

- Git, and enough network access to reach `github.com`, `index.crates.io`,
  `static.rust-lang.org`, and the Buck2 GitHub release assets.
- `rustup`. The build compiler is pinned by `rust-toolchain.toml`
  (`nightly-2026-07-12`, rustc 1.99.0-nightly `be8e82435`); rustup installs it
  on first use. `components = ["clippy", "rustfmt"]` is part of that pin
  because rustup does not add them to a freshly installed dated toolchain
  otherwise.
- `rust-script` 0.36.0. Dependency regeneration runs one checked-in
  rust-script postprocessor that materializes Reverie DBT's native source tree;
  install the pinned version with
  `cargo install rust-script --version 0.36.0 --locked`.
- The open-source [DotSlash](https://dotslash-cli.com) launcher. **On a Meta
  host, see "On a Meta host" below — the `dotslash` already on `PATH` is a
  different program and will not work.**

## Steps

```sh
git clone --recursive https://github.com/rrnewton/hermit.git
cd hermit
./bootstrap/regenerate-rust-deps
./bootstrap/buck2 build \
    reverie//:reverie-ptrace \
    reverie//:reverie-rpc-transport \
    reverie//:reverie-liteinst \
    reverie//:reverie-kvm
./bootstrap/buck2 build --keep-going shim//third-party/rust/...
./bootstrap/buck2 build //hermit-cli:hermit
```

If you already have a checkout, `git submodule update --init --recursive`
replaces the `--recursive` in the clone.

`//hermit-cli:hermit` is the green gate. The complete generated third-party
target pattern is a diagnostic rather than a gate: it includes optional and
non-host platform targets that the default Hermit binary does not use.

## Shadow release parity

The feature-complete release target is shadow-only: Cargo remains the
authoritative binary and install/resource producer. First run the existing
`build.runtime_release` command, then invoke the guarded wrapper with explicit
absolute paths:

```sh
./scripts/build-buck-release.rs \
  --dotslash /absolute/path/to/public/dotslash \
  --cargo-binary "$PWD/target/release/hermit" \
  --install-bundle "$PWD/target/install_pkg" \
  --safehermit /absolute/path/to/dev-hermit/bin/safehermit
```

The wrapper always regenerates dependencies, refuses missing release metadata
inside Rust compilation, resolves Buck through the supplied public DotSlash
launcher, and records hashes for DotSlash, the Buck descriptor, resolved Buck
executable, generated graph, binaries, resources, and the
`-llzma` link input. Before DotSlash's first probe it snapshots the launcher
into the exclusive evidence tree with stable before/copy/after hashes, uses
only that snapshot, and re-verifies it before the final receipt. Deleting or
changing the caller path cannot change the run. The wrapper likewise snapshots
the DotSlash descriptor and resolved Buck executable, then invokes only the
snapshotted Buck binary for version, build, and log decoding. Before any Cargo
probe it snapshots the caller's binary and complete install bundle into the
exclusive evidence directory. Every subsequent probe, comparison, matrix, and
receipt fact uses that tested snapshot, never the mutable caller paths. Buck
receives a second verified copy of the snapshotted install tree, and the two
complete resource manifests must be byte-identical. Before the first probe the
wrapper also snapshots the
reviewed `ROOT/bin/safehermit` plus its required
`ROOT/scripts/bounded-run-space` companion into the evidence tree, executes
only that copy, and re-verifies both tool hashes before the final receipt. The
named Cargo and Buck version, help, and host-capability outputs are retained;
receipt recomputation typed-decodes both version/host reports, compares the
help bytes exactly, and typed-decodes and compares both candidates' ptrace run
and record reports again. It also replays retained Buck event/stdout/shell-exit
semantics, re-runs the snapshotted Buck log summary, and requires exactly the
ten named candidate invocations with applied wall/cgroup safehermit reports.
After both official DBT matrices, it inventories the observed (not statically
predicted) Cargo and Buck matrix candidate invocations, rejects unexpected
tree entries and non-regular files, validates every applied wall/cgroup report,
and publishes a closed manifest binding each candidate label/identity and its
normalized case/role/argv, report/stdout/stderr hashes, and data-tree hashes.
Completeness is not inferred from equal nonzero populations: before execution,
the driver imports the official `run_matrix.py` catalog/expectation/reference
policy, pins its hermetic workdir to `/test`, and retains a source-hash-bound
expected ledger. For the current strict DBT mode that independently requires
28 TSV rows (27 selected plus the documented `pthread_lifecycle` gap), 23
ptrace references, 81 DBT candidate runs, two global probes, and therefore 106
proxy invocations per candidate. Cargo and Buck must each match that exact
case/role multiset and their normalized argv multisets must match. The final
typed receipt binds the expected-ledger name/hash/count plus the actual
manifest hash and both per-candidate counts, then recomputes all of it.
Post-behavior evidence mutation, deletion, or addition therefore refuses.
The generated Buck graph and linker-resolved `liblzma.so` cannot be redirected
to snapshots without changing Buck's project/link semantics. They instead have
stable hashes immediately before/after the build and at receipt time. This is a
fail-closed invariant for ordinary non-malicious concurrent workspace/package
changes, not an immutable-consumption claim. The final receipt has a closed
typed schema: unknown, duplicate, missing, mutated, or non-recomputable semantic
facts refuse. It publishes separate Cargo and Buck bundles only under
`ignored/buck2-phase1/`; it never writes the authoritative
`target/ci/hermit-strict` or E2E artifact pointer. A pass requires equal build
records and backend inventories, compatible ELF contracts, canonical ptrace
strict-verify and record/replay evidence through `safehermit`, and equal
duration-normalized results from the official DBT parity matrix. Buck event
evidence is retained as `.json-lines.gz` and must decode through
`buck2 log summary` before the wrapper can pass.

## Public nightly evidence and the full-parity boundary

`.github/workflows/buck2-oss-nightly.yml` is schedule/manual only and has
read-only repository permission. The original 120-minute `oss-buck2` job still
owns dependency regeneration, the four Reverie targets, the third-party
diagnostic, and `//hermit-cli:hermit`. A separate independent supplemental job
installs `rust-script` into an isolated `CARGO_HOME`, binds `PATH` to that exact
executable, runs the release driver's unit/refusal tests, and builds
`//hermit-cli:hermit-release` with explicit version, date, 12-character Hermit
SHA, and 40-character Reverie pin metadata.
The step retains a recognized `.json-lines.gz` Buck event log, decodes its
summary, and reconciles it with the retained `--show-output` stdout. The
reviewed success contract is: shell exit zero; exactly one completed CommandEnd;
exactly one Result with zero errors and the canonical release target; and
exactly one executable output path named by stdout whose canonical realpath is
under the checkout's `buck-out/`, ends in the measured configured-target layout
`hermit-cli/___hermit-release__/hermit`, and is an executable x86_64 ELF.
Before upload, a separate 10-second process-group-bounded CLI-only probe decodes
that candidate's typed `version --json` and requires the exact metadata and
`dbt`/`e9patch`/`sabre` feature facts. This rejects an executable decoy without
making a guest-execution or behavioral-parity claim.
Measured pinned-Buck logs serialize `CommandEnd.is_success=false` even for
successful builds, so that field is retained and explicitly labeled
`advisory-known-inconsistent`; it is neither hidden nor used to override the
canonical Result/shell/output evidence. One portable Rust operation examines
every prospective regular artifact file through one stable read that returns
the raw bytes, filesystem identity, and digest. Event logs are decompressed
from those same captured raw bytes; the receipt and `SHA256SUMS` are derived
from that same map, and a deterministic pinned `tar`/`flate2` writer creates
the archive directly from the captured bytes. The operation decodes that
in-memory archive, requires exact member bytes and checksum semantics,
re-enumerates the source population, stable-reverifies every original
identity/hash, and only then atomically publishes the read-only archive and
sidecar receipt. There is no scanner-to-packager process boundary and no source
payload reread during packaging. Symlinks, special files, unsafe/non-UTF-8
names, credential markers, coherent manifest rewrites, and late files all
refuse. Non-green build/event evidence is still scanned and
packaged before the supplemental step propagates failure; scanner refusal
suppresses upload. `safe-to-upload` means secret-scan and artifact safety only,
never parity or pass; the typed evidence verdict controls step success. Shell
steps explicitly blank known token carriers, while
upload actions retain their Actions runtime authentication. The legacy
third-party artifact name and downloaded top-level `buck2-third-party.log`
payload are preserved. A second combined mode stable-reads and scans the log,
then materializes the exact historical two-file staging population (captured
`buck2-third-party.log` plus `SHA256SUMS`) directly from those bytes, verifies
the staged digest internally, and re-verifies the original identity before
success. No shell or second process reads a mutable scanner receipt/digest.
Only after that check may the legacy step print its final 200 diagnostic lines,
preserving the historical log-output contract without printing unscanned content. The
immediate upload is a trusted Actions-step handoff: it assumes no malicious
same-user process rewrites read-only runner files between the final verification
and `upload-artifact`; the helper does not claim immutable consumption across
that process boundary.
The workflow checker exact-binds reviewed critical blocks. Its FNV hashes are
drift tripwires only; semantic validation and exhaustive mutation tests remain
the enforcement logic.
This supplemental job is release-build evidence only, not Cargo/Buck
behavioral-parity evidence and not a speedup claim.

A separate public full-parity job may call `build-buck-release.rs` only when
all of the following are available without repository secrets:

- the authoritative same-SHA Cargo release binary and complete
  `target/install_pkg` bundle;
- the public pinned DotSlash launcher and a source-controlled or
  content-addressed, independently reviewable public `safehermit` plus
  `bounded-run-space` execution bundle;
- delegated cgroup v2 controls needed by `safehermit` and `dagrun`, a real
  btrfs filesystem for the isolation contract, and accessible PMU retired-
  branch counters required by strict Hermit execution; and
- read-only repository permissions, public-network-only dependencies, a
  credential-marker scan before artifact publication, and no authoritative
  pointer or `target/ci/hermit-strict` writes.

That job is not currently enabled. There is no reviewed public
`safehermit`/`bounded-run-space` bundle; both currently live only in the private
parent workspace. GitHub-hosted Ubuntu runners also do not promise the required
btrfs mount, delegated cgroup controls, or PMU access. Copying private launcher
code into this repository, adding a PAT, silently substituting `timeout`, or
skipping hardware/isolation checks would weaken the validation contract.

Once an owner-reviewed public execution bundle is published with immutable
hashes, the supplemental job is mechanically runnable on a public self-hosted
runner labeled for x86_64 Linux, PMU access, btrfs quotas, and delegated cgroup
v2. It should use only `workflow_dispatch`/the reviewed schedule, retain
`permissions: contents: read`, set `persist-credentials: false`, verify the
bundle hashes before use, build the authoritative same-SHA Cargo artifacts,
and invoke the wrapper with absolute paths. Its first step must fail closed
with a retained prerequisite report unless PMU, btrfs quota/readback, cgroup
delegation, and passwordless bounded-run-space cleanup all work. Its last step
must run the same single-process captured-byte scan-and-package operation before
upload. Absence of a
runner or public bundle is `blocked`, never parity-green.

## On a Meta host

A Meta devserver needs five things the steps above do not mention. All five are
host facts rather than repository defects; a machine with direct internet
access and no internal `dotslash` needs none of them.

**Every network-touching command needs `with-proxy`** — the clone, the
crates.io index fetch, the Buck2 release download, and any rustup toolchain
install.

**`/usr/bin/dotslash` is the internal DotSlash2 and cannot read this
descriptor.** `./bootstrap/buck2` fails with:

```
dotslash error: problem with .../bootstrap/buck2
caused by: failed to parse DotSlash file
caused by: missing field `scheme`
```

That is a launcher-dialect difference, not a defect in the pin. Internal
descriptors carry a per-platform `scheme` field (for example `"scheme": "cas"`)
and slash-form platform keys (`linux/x86_64`); the public schema has neither,
using `providers[].url` and hyphen-form keys (`linux-x86_64`). Fetch a public
launcher and invoke it explicitly rather than putting it on `PATH`, so nothing
internal is shadowed:

Unpack it **outside** the checkout, so it does not show up as untracked files:

```sh
mkdir -p ~/.local/dotslash && cd ~/.local/dotslash
with-proxy curl -sSL -o ds.tgz \
  https://github.com/facebook/dotslash/releases/download/v0.5.9/dotslash-linux-musl.x86_64.v0.5.9.tar.gz
tar xzf ds.tgz && rm ds.tgz     # yields ~/.local/dotslash/dotslash
cd -
with-proxy ~/.local/dotslash/dotslash ./bootstrap/buck2 build reverie//:reverie-ptrace
```

**`regenerate-rust-deps` needs two Cargo environment variables.** Without the
first, the pinned Reindeer's bundled libcurl does not find the system CA bundle
and fails with `[60] SSL peer certificate ... unable to get local issuer
certificate`, even though system `curl` reaches `index.crates.io` normally.
Without the second it fails with `[7] CONNECT tunnel failed, response 407`,
because the host's `~/.cargo/config.toml` sets `proxy = "fwdproxy:8080"` with
no URL scheme:

```sh
CARGO_HTTP_CAINFO=/etc/pki/tls/certs/ca-bundle.crt \
CARGO_HTTP_PROXY=http://fwdproxy:8080 \
  ./bootstrap/regenerate-rust-deps
```

## Pinned versions

The wrappers use immutable versions rather than live branch tips:

- Buck2 release `2026-08-01`, through Buck2's upstream DotSlash descriptor with
  a BLAKE3 digest and size for each supported platform. The descriptor's size
  and digest describe the compressed `.zst` artifact, not the decompressed
  binary in the cache — those two numbers differing is expected.
- Reindeer `e3d72748131d3a70378055f091e0647c1edad85e`
- Reindeer's own Rust toolchain `nightly-2026-05-22`
- The build compiler, `nightly-2026-07-12` in `rust-toolchain.toml`

The compiler pin matters as much as the others. The shim uses
`system_rust_toolchain`, which runs whatever `rustc` is on `PATH`; under rustup
that resolves through `rust-toolchain.toml`. While that file said `nightly`, a
reviewer building on a different day got a different compiler, which left every
other pin here without effect.

Note that `reverie/rust-toolchain.toml` may select a different compiler for
standalone Cargo work. Under Buck2 this is inert — actions run from the outer
project root, so the Hermit pin governs the whole build.

The first Reindeer invocation downloads the pinned source revision, installs the
pinned Rust toolchain if needed, and compiles Reindeer into the user cache
(about 1m25s cold). Set `HERMIT_BUCK2_TOOL_CACHE` to place that cache
elsewhere. DotSlash downloads and verifies the platform-specific Buck2 release
binary; `DOTSLASH_CACHE` relocates its cache.

`regenerate-rust-deps` starts without generated dependency output, vendors the
versions in the tracked root `Cargo.lock`, generates
`shim/third-party/rust/BUCK` twice, and refuses the result if two consecutive
outputs differ. It also refuses changes to the lockfile. Both Hermit and
Reverie consume this graph. The repository-root `.gitignore` excludes generated
paths. Those patterns must not move into `shim/.gitignore`: pinned Reindeer
reads ignore files through the shim cell root and would otherwise generate
empty crates.

## What a reproduction should produce

Measured 2026-09-20 on x86_64 Linux with warm tool and crate caches:

| Step | Result |
|---|---|
| `regenerate-rust-deps` | exit 0, 300 crates; `Cargo.lock` SHA-256 `02e7643d7a5e8339bad982f9443d1289a02197bc0a8b130d2a8c5b37d686b0e2`; generated `BUCK` SHA-256 `4a73386cc01c33b308f8b915a2757b57dfd37912251895da50749f9fbbe31b7d` |
| build Reverie's ptrace, RPC transport, LiteInst, and KVM libraries together | exit 0 |
| `build //hermit-cli:hermit` | exit 0 |

The two cells do not compile separate copies of third-party crates.
`.buckconfig` maps the `reverie_shim` cell onto the Hermit shim cell, so
Reverie's BUCK files resolve their third-party crates through the same generated
targets as Hermit. `shared-cell-aliases.txt` provides the unversioned names used
by Reverie's hand-written BUCK targets.

No shared action-cache performance measurement has yet been made. A local
successful build proves target compatibility only, not the vision's claimed
cross-worktree benefit.
