# Detcore for the Narf kernel

Generated; do not edit. This is Detcore, the deterministic execution engine of
Hermit (https://github.com/facebookexperimental/hermit), packaged for the Narf
kernel, which builds it without std.

- Hermit commit: 9abfa76862616a6d98e21d79a1f8cfeabf642ed4
- Reverie commit: c6c047b63e82dab57943cb9215413e1c14f465cf (https://github.com/rrnewton/reverie)
- rand_pcg 0.10.2: the crates.io package (sha256 caa0f4137e1c0a72f4c651489402276c8e8e1cf081f3b0ba156d2cbeef09e86a) with
  `default-features = false` added to its serde dependency

The sources are Hermit's, unchanged, except for the Reverie rev in
detcore-libc/Cargo.toml. The manifests under kernel/ replace Hermit's
generated detcore/Cargo.toml and detcore-model/Cargo.toml, which describe the
host build.

To depend on it, take `hermit-detcore` from this commit and patch crates.io's
`rand_pcg` to this commit, and take every Reverie crate from the Reverie
commit above.

To regenerate, in a Hermit checkout at the commit above:

    scripts/export-detcore-kernel-crates.sh --reverie-rev c6c047b63e82dab57943cb9215413e1c14f465cf --out DIR
