# Reproducing the OSS Buck2 build

The OSS Buck2 project is generated from Hermit's authoritative root
`Cargo.toml` and tracked `Cargo.lock`. Reverie remains a separate Buck cell and
generates its dependencies from its own root manifest and tracked lockfile.
Generated Rust `BUCK` files and vendored crate sources remain ignored because
they can be regenerated.

From a fresh recursive checkout, run:

```sh
git submodule update --init --recursive
./bootstrap/regenerate-rust-deps
./bootstrap/buck2 build shim//third-party/rust/...
./bootstrap/buck2 build reverie//:reverie-ptrace
./bootstrap/buck2 build //hermit-cli:hermit
```

Prerequisites are Git, rustup, and the open-source
[DotSlash](https://dotslash-cli.com) launcher.

The wrappers use immutable versions rather than live branch tips:

- Buck2 release `2026-08-01`, through Buck2's upstream DotSlash descriptor
  with a BLAKE3 digest and size for each supported platform
- Reindeer `e3d72748131d3a70378055f091e0647c1edad85e`
- Reindeer's Rust toolchain `nightly-2026-05-22`

Hermit's own `rust-toolchain.toml` still selects floating `nightly`; this
bootstrap does not make the product compiler reproducible.

The first Reindeer invocation downloads the pinned source revision, installs
the pinned Rust toolchain if needed, and compiles Reindeer into the user cache.
Set `HERMIT_BUCK2_TOOL_CACHE` to place that cache elsewhere. DotSlash downloads
and verifies the platform-specific Buck2 release binary.

`regenerate-rust-deps` starts without generated dependency output, vendors the
versions in each tracked `Cargo.lock`, generates each
`shim/third-party/rust/BUCK` twice, and refuses the result if two consecutive
outputs differ. It also refuses changes to either tracked lockfile. The
repository-root `.gitignore` files exclude generated paths. Those patterns must
not move into `shim/.gitignore`: pinned Reindeer reads ignore files through the
shim cell root and would otherwise generate empty crates.

The historical build reaches `//hermit-cli:hermit` and then stops because the
Hermit and Reverie cells compile separate copies of third-party Rust crates.
Types crossing the cell boundary consequently have distinct trait identities,
first observed when Reverie's `Sysno` met Hermit's `serde` traits. Resolving
that requires an owner decision about one third-party graph versus Hermit's
hermetic Reverie pin; this bootstrap deliberately does not choose one.

No shared action-cache performance measurement has yet been made. A local
successful build proves target compatibility only, not the vision's claimed
cross-worktree benefit.
