# Hermit + DynamoRIO packaging design

Status: research + design (P2). Date basis: 2026-07-22. Companion to
`ai_docs/transient/dbi-backend-results.md` (DBI backend test/benchmark results).

## Problem

`hermit run --backend dbi` currently depends on three environment variables set
by hand:

- `DYNAMORIO_HOME` / `DynamoRIO_DIR` — gates backend availability
  (`hermit-cli/src/lib.rs:181`).
- `HERMIT_DRRUN` — path to `drrun` (`backends.rs:115`).
- `HERMIT_DBI_CLIENT` — path to `libreverie_dbi_client.so` (`backends.rs:122`).

This "bring your own 501 MB DynamoRIO install and export env vars" model is not
shippable. This doc answers the packaging questions with measured evidence and
recommends a layout + installer strategy.

## Measured facts (this host: AMD EPYC, Linux 6.13, DR 11.91)

### Q1. Is `drrun` a single binary, or does it need `lib64/`?

Not single. `ldd drrun` shows only `libc`/`ld`, but at **runtime** `drrun`
`LD_PRELOAD`s `libdynamorio.so` + `libdrpreload.so`, which it locates **relative
to its own path** at `bin64/../lib64/release/`. So `drrun` needs the `lib64/`
tree beside it. (`strings drrun` shows it probes `lib{32,64}/{debug,release}/
libdynamorio.so`; the harmless "not a valid DynamoRIO root" warnings we see come
from the missing lib32/debug variants — release/lib64 satisfies it.)

### Q2. Can DynamoRIO be statically linked into hermit?

No — not in a way that helps. DR works by **injecting `libdynamorio.so` into the
guest process** (via `drrun`'s LD_PRELOAD path), not by running inside the
launcher. Linking DR into the `hermit` binary would not put DR where it needs to
be (the guest). DR does ship `libdynamorio_static.a` (38 MB) and
`libdrdecode.a` (11 MB), but those are for building a **standalone statically
linked DR client executable**, which is a different execution model than
hermit's `drrun` + client `.so`.

What *is* worth doing: static-link the DR **extension** libs
(`drx`/`drmgr`/`drreg`/`drwrap`) into `libreverie_dbi_client.so` so the client
is one self-contained `.so` instead of pulling four loose `.so` files at runtime.
That shrinks the loose-file count but not the core (`libdynamorio.so` +
`libdrpreload.so` + `drrun` still ship separately).

### Q3. Minimal DR footprint for hermit's use case

**3.1 MB, 6 files** (verified: this pruned tree's `drrun` runs `/bin/echo`
standalone), versus the **501 MB** full install:

| file | size | why |
|---|---:|---|
| `bin64/drrun` | 737 KB | launcher / injector |
| `lib64/release/libdynamorio.so` | 2.1 MB | core DR runtime (injected into guest) |
| `lib64/release/libdrpreload.so` | 44 KB | LD_PRELOAD injection shim |
| `ext/lib64/release/libdrx.so` | 78 KB | client-linked extension |
| `ext/lib64/release/libdrmgr.so` | 88 KB | client-linked extension |
| `ext/lib64/release/libdrreg.so` | 58 KB | client-linked extension |
| `ext/lib64/release/libdrwrap.so` | 58 KB | client-linked extension |

Plus **hermit's own** artifacts (from the `reverie-dbi` build, not DR):
`libreverie_dbi_client.so` (~29 KB) and `libreverie_dbi.so` (the Rust runtime it
`NEEDED`s). The client's runtime dependency closure (from `readelf -d`) is
exactly: `libdrx, libdrwrap, libdrreg, libdrmgr, libdynamorio` (+ its own
`libreverie_dbi.so`, `libc`, `libgcc_s`).

Everything else in the 501 MB install is **build-time or unused**: `tools/`
(340 MB — drcachesim etc.), `samples/` (13 MB), `include/` (2.5 MB, headers),
`cmake/` (build config), `*.debug` symbol files, and all the `.a` static libs.

### The real blocker: RPATH relocatability

`readelf -d libreverie_dbi_client.so` shows an **absolute, build-time** RPATH:

```
RPATH  /home/.../slot12/reverie/target/debug:/tmp/dynamorio-cpuid/install/ext/lib64/release:/tmp/dynamorio-cpuid/install/lib64/release
```

Consequences:
1. The shipped client would look for DR under a path that only existed on the
   build machine. Today it happens to still resolve because `/tmp/dynamorio-cpuid`
   survives on this host — i.e. the client is loading DR from a **`/tmp` build
   dir**, *not* from `DYNAMORIO_HOME`. That is latent breakage (a reboot / tmp
   cleanup kills it).
2. `RPATH` (as opposed to `RUNPATH`) takes precedence over `LD_LIBRARY_PATH`, so
   you cannot fix a bundled install by exporting `LD_LIBRARY_PATH`. The client
   **must** be built with an `$ORIGIN`-relative RPATH.

Root cause in-tree: `reverie-dbi/native/CMakeLists.txt` sets `BUILD_RPATH` to the
absolute OUT_DIR DR path and `SKIP_BUILD_RPATH OFF`, and the client is used
straight from the build tree. There is no `INSTALL_RPATH` / install step.

## Recommended design

### Layout: `hermit` binary + a resources directory it finds relative to itself

```
<prefix>/
  bin/hermit
  lib/hermit/dynamorio/            # the pruned 3.1 MB DR runtime
    bin64/drrun
    lib64/release/{libdynamorio.so, libdrpreload.so}
    ext/lib64/release/{libdrx,libdrmgr,libdrreg,libdrwrap}.so
  lib/hermit/dbi/                  # hermit's own DBI artifacts
    libreverie_dbi_client.so       # RPATH = $ORIGIN + $ORIGIN/../dynamorio/...
    libreverie_dbi.so
```

`hermit` resolves these **relative to its own executable path** (read
`/proc/self/exe`, walk to `../lib/hermit/...`), with the existing env vars kept
as overrides:

- `HERMIT_DRRUN` → default `../lib/hermit/dynamorio/bin64/drrun`
- `HERMIT_DBI_CLIENT` → default `../lib/hermit/dbi/libreverie_dbi_client.so`
- `DYNAMORIO_HOME` availability check → satisfied by the bundled dir; env still
  honored first.

This removes the hand-set-env requirement (Q4) and keeps the escape hatch for a
system DR. It is a small, additive change to `backends.rs`/`lib.rs` (resolve a
bundled default before erroring), plus a build/packaging step — no change to the
determinism engine.

### Making the client relocatable (prerequisite for any bundle)

In `reverie-dbi/native/CMakeLists.txt`, set an install step with
`INSTALL_RPATH '$ORIGIN:$ORIGIN/../dynamorio/ext/lib64/release:$ORIGIN/../dynamorio/lib64/release'`
and `INSTALL_RPATH_USE_LINK_PATH OFF`, then install the client into the bundle.
Optionally static-link the DR extensions into the client (Q2) to drop the four
loose `ext` `.so` files. Verify with `readelf -d` that the shipped client has
`$ORIGIN` RPATH and no absolute paths.

### Installer strategy (Q5/Q6)

`cargo install hermit` alone cannot ship 3 MB of prebuilt binary DR blobs (crates
are source-only). Three options, in recommended order:

1. **Release tarball / OS package (recommended).** A `hermit-<ver>-x86_64-linux`
   tarball (or `.deb`/`.rpm`) laying out `bin/hermit` + `lib/hermit/...` as
   above. CI builds DR once (the `reverie-dbi` `build.rs` already builds+installs
   DR into `OUT_DIR/dynamorio-install`), the packaging step **prunes it to the
   3.1 MB subset**, fixes the client RPATH, and archives it. Smallest, most
   predictable, no per-user build. Total artifact ≈ hermit binary + ~3.2 MB.
2. **`cargo install` + post-install fetch/build.** `cargo install hermit` then
   `hermit setup-dbi` builds/downloads and installs the pruned DR into a
   user data dir (`$XDG_DATA_HOME/hermit/dynamorio`). Reuses the existing
   `build.rs` DR build; needs network + a compiler on first use. Good for
   developers, heavier for end users.
3. **Self-extracting fat binary.** Embed the ~3.2 MB pruned tree in the `hermit`
   binary and extract to a cached dir on first `--backend dbi` use. One file to
   ship; costs ~3 MB of binary bloat for all users (even non-DBI) and adds
   extract/version-check logic. Use only if a single-file distribution is a hard
   requirement.

The ptrace backend needs none of this, so DBI packaging should be **optional**:
the base `hermit` works without the `lib/hermit/dynamorio` dir; `--backend dbi`
prints an actionable "run `hermit setup-dbi`" error when it is absent (the
availability plumbing already exists from PR #181).

## Suggested phasing (all follow-ups, not done here)

1. Make the client relocatable (`$ORIGIN` RPATH + install step) — unblocks
   everything; small CMake change.
2. Add DR-tree pruning to the build/packaging (the 6-file, 3.1 MB subset).
3. Teach `hermit` to resolve `drrun`/client relative to `/proc/self/exe`, env as
   override.
4. Ship a release tarball (option 1); add `hermit setup-dbi` for the
   `cargo install` path (option 2).

## Appendix: reproduction

```bash
DR=~/dynamorio/install
# minimal tree (3.1 MB) — verified to run:
mkdir -p mindr/{bin64,lib64/release,ext/lib64/release}
cp $DR/bin64/drrun mindr/bin64/
cp $DR/lib64/release/{libdynamorio.so,libdrpreload.so} mindr/lib64/release/
cp $DR/ext/lib64/release/{libdrx,libdrwrap,libdrreg,libdrmgr}.so mindr/ext/lib64/release/
mindr/bin64/drrun -- /bin/echo minimal-DR-works   # prints "minimal-DR-works"

# the relocatability problem:
readelf -d <...>/libreverie_dbi_client.so | grep -Ei 'rpath|needed'
```
