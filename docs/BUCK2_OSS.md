# Reproducing the OSS Buck2 build

The OSS Buck2 project is generated from Hermit's authoritative root
`Cargo.toml` and tracked `Cargo.lock`. Reverie remains a separate Buck cell and
generates its dependencies from its own root manifest and tracked lockfile.
Generated Rust `BUCK` files and vendored crate sources remain ignored because
they can be regenerated.

## Prerequisites

- Git, and enough network access to reach `github.com`, `index.crates.io`,
  `static.rust-lang.org`, and the Buck2 GitHub release assets.
- `rustup`. The build compiler is pinned by `rust-toolchain.toml`
  (`nightly-2026-07-12`, rustc 1.99.0-nightly `be8e82435`); rustup installs it
  on first use. `components = ["clippy", "rustfmt"]` is part of that pin
  because rustup does not add them to a freshly installed dated toolchain
  otherwise.
- The open-source [DotSlash](https://dotslash-cli.com) launcher. **On a Meta
  host, see "On a Meta host" below — the `dotslash` already on `PATH` is a
  different program and will not work.**

## Steps

This work lives on the `revive/buck2-oss-901` branch, not on `main`. `main` has
no `BUCK` files at all, so cloning the default branch leaves nothing for the
procedure to act on and produces no error explaining why.

```sh
git clone --recursive --branch revive/buck2-oss-901 \
    https://github.com/rrnewton/hermit.git
cd hermit
./bootstrap/regenerate-rust-deps
./bootstrap/buck2 build reverie//:reverie-ptrace
./bootstrap/buck2 build --keep-going shim//third-party/rust/...
./bootstrap/buck2 build //hermit-cli:hermit
```

If you already have a checkout, `git submodule update --init --recursive`
replaces the `--recursive` in the clone.

`reverie//:reverie-ptrace` is the green gate. The complete generated
third-party target pattern is a **diagnostic, not a gate**: it is expected to
report the Windows-only `winapi-0.3-build-script-run` analysis exception plus
`reverie-dbt-0.2` and `reverie-sabre-0.2`, whose Cargo build-script outputs are
not yet represented in Reindeer fixups. Neither optional package is on the
default `//hermit-cli:hermit` dependency path. `//hermit-cli:hermit` itself
stops at a known architecture boundary described at the end of this document.

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

Note that `reverie/rust-toolchain.toml` pins a different nightly
(`nightly-2026-07-29`) for its own reasons. Under Buck2 this is inert — actions
run from the outer project root, so the Hermit pin governs the whole build —
but a `cargo` command run from inside `reverie/` will use the Reverie pin.

The first Reindeer invocation downloads the pinned source revision, installs the
pinned Rust toolchain if needed, and compiles Reindeer into the user cache
(about 1m25s cold). Set `HERMIT_BUCK2_TOOL_CACHE` to place that cache
elsewhere. DotSlash downloads and verifies the platform-specific Buck2 release
binary; `DOTSLASH_CACHE` relocates its cache.

`regenerate-rust-deps` starts without generated dependency output, vendors the
versions in each tracked `Cargo.lock`, generates each `shim/third-party/rust/BUCK`
twice, and refuses the result if two consecutive outputs differ. It also refuses
changes to either tracked lockfile. The repository-root `.gitignore` files
exclude generated paths. Those patterns must not move into `shim/.gitignore`:
pinned Reindeer reads ignore files through the shim cell root and would
otherwise generate empty crates.

## What a reproduction should produce

Measured 2026-08-21 from a fresh recursive clone at `e95ab8c9`, reverie
`868d46cf`, on x86_64 Linux with cold DotSlash and tool caches:

| Step | Result |
|---|---|
| `regenerate-rust-deps` | Hermit `Cargo.lock` `bf71543c…`, `BUCK` `493aa548…`; Reverie `Cargo.lock` `69960ec2…`, `BUCK` `fba4eb29…`; working tree clean |
| `build reverie//:reverie-ptrace` | exit 0, 24.8s, `libreverie_ptrace-0229eb76.rmeta` |
| `build --keep-going shim//third-party/rust/...` | exit 3, 1m31s; red on `reverie-dbt-0.2` and `reverie-sabre-0.2` plus the `winapi-0.3-build-script-run` analysis exception; 14 incompatible targets skipped |

Those four generation hashes were produced independently in three different
checkouts, so generation is deterministic across clones and not merely
repeatable in one worktree.

**`14 incompatible targets skipped` is the number the pinned Buck2 reports. A
different Buck2 binary running the identical command reports 23, and both are
correct.** Meta-internal Buck2 `083174567c29` says 23 where the pinned public
release 2026-08-01 says 14; the earlier validation of this branch used the
internal binary, because the internal DotSlash could not run the pinned
descriptor. Nothing else about the build differs between the two: same two
failing compilation targets, same `winapi-0.3-build-script-run` analysis error,
same nine `dep_only_incompatible_version_two` soft errors, `BUILD ERRORS (2)`
either way. The two binaries bundle different preludes — `prelude` is a bundled
external cell — so `prelude//platforms:default` configures differently and the
two runs share no configuration hash at all.

The difference of nine is accounted for by the nine per-platform
`winapi-0.3-build-script-build-{linux-arm64, linux-riscv64, linux-x86_64,
macos-arm64, macos-x86_64, wasi, wasm32, windows-gnu, windows-msvc}` targets,
each of which is incompatible only because a transitive dep is, and each of
which both binaries report identically as a soft error saying "will be error in
future". The internal binary appears to count them in the skipped total; the
pinned release appears to leave them to the soft-error channel.

That account rests on arithmetic (23 − 14 = 9), on those nine soft-error names
being identical in both logs, and on the printed list's first three and last
three entries being unchanged between the two runs — `winapi` sorts before
`windows`, so nine extra entries would land in the hidden middle. **It does not
rest on an enumerated diff of the two lists**, because Buck2 truncates the
printed list to first-three-and-last-three and `ctargets` fails on the same
`winapi-0.3-build-script-run` analysis error. Nobody has read Buck2's source to
confirm the counting change between the two versions. Quote 14 when following
this document, since the pinned release is what it tells you to run.

**The first `shim//third-party/rust/...` build in a cold checkout can report a
third failure that is not real.** Once in five runs of that command, a freshly
cloned tree additionally failed
`gh_facebook_buck2_shims_meta//third-party/rust:serde_core-1-build-script-build`
with a missing intermediate input:

```
transitive_dependency_symlinks.py: error: argument --artifacts: can't open
  '...__serde_core-1-build-script-build__/.../XIPL-depslink-symlinked_dirs.json'
```

That is an action-graph symptom rather than a compile error, and it cleared on
both immediate re-runs of the identical command in the same checkout. If you
see three failures instead of two, re-run before drawing a conclusion; the red
set in the table above is what the branch actually reproduces.

## Known stopping point

The current build reaches `//hermit-cli:hermit` and then stops because the
Hermit and Reverie cells compile separate copies of third-party Rust crates.
Types crossing the cell boundary consequently have distinct trait identities,
currently observed when Reverie's `Sysno` meets Hermit's `serde::Serialize` and
`serde::Deserialize` traits in `detcore-model`. Resolving that requires an owner
decision about one third-party graph versus Hermit's hermetic Reverie pin; this
bootstrap deliberately does not choose one.

No shared action-cache performance measurement has yet been made. A local
successful build proves target compatibility only, not the vision's claimed
cross-worktree benefit.
