# Repository-local Rust source launcher review — 2026-08-06

An independent architecture audit challenged how a source checkout selected
Rust executables. Published crates and `cargo install` were out of scope: this
review concerns only the repository-local `rs/bin` command surface.

## Finding

`setup rs` copied compiled executables into ignored `rs/bin` paths. The common
engine resolver verified their source and artifact provenance, but a direct
`rs/bin/<command>` invocation bypassed that guard. Agents could therefore run an
older copied executable after source changed.

Making each link execute its crate `main.rs` through `rust-script` was evaluated
and rejected. All six paired-tool entrypoints delegate into library crates, while
`rust-script` 0.36 decides whether to reuse its executable from the entrypoint
script and generated manifest timestamps. A library/module change with an
unchanged `main.rs` can consequently reuse stale code. Its generated package
also does not use the workspace's lockfile and profile as the package itself.

## Resolution

- All six `rs/bin/<command>` paths are tracked links to one `cargo-runner`.
- Every normal invocation asks Cargo to check the real locked release workspace
  cache, then replaces the launcher process with the resulting binary.
- One checkout-wide lock outside `rs/target` serializes verification, cleanup,
  build, stamping, and snapshot publication. Both clean paths take that lock
  before deleting the Cargo cache.
- The host target and target directory are explicit Cargo arguments, preventing
  ambient environment or Cargo configuration from redirecting the checked build.
- Source and binary hashes are stored beside the cached target executable.
- Missing, malformed, binary-mismatched, non-executable, or source-stale cache
  state causes a package-scoped clean rebuild. Source-content changes force that
  rebuild even when file modification times were preserved.
- Source changes during the build or stamp window retry and fail closed after a
  bounded number of attempts.
- Normal launch uses a hash-verified, named content-addressed copy outside the
  deletable Cargo cache. This preserves `current_exe()` re-execution semantics.
- The differential harness revalidates provenance under the checkout lock, then
  uses its own immutable executable copy for the subprocess corpus.

The tracked launchers are outside every crate archive. Registry packages still
install ordinary standalone executables declared by their Cargo manifests.

## Adversarial controls

Hermetic tests using the real resolver, fingerprint helper, and Cargo launcher
prove that:

1. changing only delegated library source rebuilds an unchanged `main.rs`, even
   when the library file's exact modification time is restored;
2. a replaced executable without matching provenance is rebuilt rather than run;
3. a missing stamp or removed executable mode also forces repair;
4. invalid newer source never falls back to the previous Rust binary or Python;
5. unchanged source retains identical target bytes and modification time;
6. ensure-only resolution returns the verified target without running the tool;
7. ambient environment and Cargo-config target defaults cannot redirect a build;
8. unrelated caller-directory Cargo configuration cannot affect the build, while
   the final command still receives the caller's working directory; and
9. a launched executable can resolve and re-execute its own stable path;
10. a coordinated clean waits for an in-flight build and cannot remove the Cargo
    cache before the stable launch snapshot is published; and
11. changes to nonignored, untracked Rust source invalidate the cache even when
    modification times are preserved; and
12. a source-enumeration command that emits partial output and then fails cannot
    bless or execute a cached binary.

The provenance files are corruption and staleness controls, not cryptographic
authentication. A process that can rewrite both ignored artifact bytes and their
adjacent hashes is inside the checkout owner's trust boundary. The launcher does
not claim to defend against that actor.

The repository topology test additionally requires all six Rust command links
to resolve to the executable tracked launcher.
