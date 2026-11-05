# Packaging and documentation adversarial review — 2026-08-06

## Review provenance

The initial artifact review was recorded with the native implementations in commit
`5ef91c55036227b5dc2997ef069784f337b4cc5e`. Three independent, read-only completion audits then
reviewed baseline `3de72236cb33929fc5a0be3014d7a354f2c5938a` from separate perspectives:

- package documentation and public API completeness;
- common-source and symlink topology plus release-archive behavior;
- implementation inventory, behavioral parity evidence, and installer claims.

The auditors reported the gaps below before implementation. Release validation runs on Linux
x86-64 with the declared Python 3.10 and Rust 1.85 minimums also exercised in CI.

A second three-way, read-only audit of published completion candidate
`6ddd9bca7cbed6387a407a0ac7760626e9be2dcb` challenged the finished package surface, parity
claims, and artifact checks. It found the gaps recorded in items 8–10; each now has an
executable regression check.

Cross-review of published follow-up `e703d82ac7c5d2667f8164f3d9a7bc9bdd06c253` then caught a
too-broad API removal and summary action schemas that Rust advertised but did not enforce. The
public library helper is preserved as described in item 10; strict CLI parity is recorded in the
safe-runner review.

A final paired-port and installer audit covered the newly added `herdr-run`, repository-local Rust
launchers, all rendered package documents, and every release artifact. Its behavioral and security
findings are recorded in `reviews/herdr-run.md` and `reviews/rust-source-launchers.md`.

## Findings resolved

1. `wrkviz` was source-only and documented a broken aggregate install. It now has an
   independent manifest, README, user guide, license, command entry point, wheel, sdist, and isolated
   artifact smoke test. Its explicit exception is Rust parity, not package quality.
2. Sixteen paired package documents, two timeline documents, and package licenses were duplicated
   regular files. Paired editions now render under `common/docs/<tool>/rendered/`; all package docs
   and all nine package licenses are exact checked symlinks to authoritative sources.
3. Release checks covered wheels but not source distributions. The Python checker now builds both,
   rejects links inside either artifact, rebuilds a wheel from each sdist, compares every packaged
   module to the differential-tested source, installs the rebuilt wheel alone, blocks network access,
   and smokes every command.
4. Python license metadata used a deprecated table and classifier. All five distributions use PEP
   639 metadata with a backend version that implements it.
5. Public API checks ignored missing documentation and implementation-history prose. Every Python
   public module/class/function and every Rust public item is now documented; package checks reject
   missing docs, sibling languages, source paths, unrelated products, templates, provenance, and
   development-history language.
6. The workspace claimed Rust 1.82 while locked dependencies required 1.85. The declared MSRV is
   now 1.85, inherited by all crates, compiler-enforced in a dedicated CI job, and used to test and
   package every crate.
7. Package checks did not prove that differential-tested code was what users installed. Wheel and
   crate inspection now compares every packaged source module byte-for-byte with the checked source;
   artifact startup also verifies installed origin and absence of sibling packages.
8. Declared non-Python resources were checked only for presence. Every declared wheel and sdist
   resource is now byte-compared with its authoritative source, direct and sdist-rebuilt wheels must
   agree, and a deliberately corrupted `timeline-core.js` wheel proves the check fails closed.
9. The repository dispatcher and dependency smoke test omitted the packaged `cpuset-alloc` companion
   command. `./bin/cpuset-alloc` now shares the tracked resolver, and all seven Python entry-point
   commands receive the dependency-free startup probes.
10. `dagrun.analyze` mixed a stable public `summarize` library helper with an undeclared Python-only
    `main()`. Removing the module would break the package API, so the helper remains
    documented and re-exported while only the CLI-shaped surface was retired. A compatibility test
    pins the helper, and a manifest-derived test rejects any public `main()` not backed by a declared
    console entry point.
11. `herdr-run` arrived as a standalone Python change while this audit was in flight. It now has an
    independent Rust crate and binary, a Python distribution, shared templates with standalone
    rendered editions, isolated artifact checks, 106-case behavioral differential, and a dedicated
    adversarial review.
12. Copied binaries under `rs/bin` could remain stale after source changed. The initial five paths are now
    tracked links to one source-aware Cargo launcher with locked provenance validation and stable
    content-addressed execution snapshots. Published crate binaries remain ordinary `cargo install`
    targets.
13. `herdr-agent` was package-published only from the Python distribution and bypassed the command
    inventory, common docs, Rust launcher, dependency smoke, and differential. It is now the sixth
    paired command, shares one installed guide across packages, and has native artifact and
    black-box parity checks.

## Executable evidence

```sh
python3 scripts/embed_userguides.py --check
make check-python-packages
make check-rust-packages
make cross
```

The documentation check covers 16 rendered paired documents, four standalone document checks,
and 29 exact package links. Artifact checks cover five wheels, five sdists, five wheels
rebuilt from sdists, and four registry crates. The repository CI additionally runs the full test,
strict typing, formatting, Clippy, Python 3.10, and Rust 1.85 contracts.
