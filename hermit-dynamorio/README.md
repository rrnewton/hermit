# hermit-dynamorio

`hermit-dynamorio` is Hermit's optional DynamoRIO/DBT payload. The flagship
Hermit binary keeps DBT dispatch glue but does not link DynamoRIO or
`reverie-dbi`.

## Install

```console
cargo install --locked hermit-dynamorio
```

This is a source install. It requires CMake and a C/C++ toolchain, builds the
pinned curated DynamoRIO source in Cargo's `OUT_DIR`, and embeds the stripped
runtime payload in the installed helper. It does not download a prebuilt bundle
at build time or runtime. Clean DynamoRIO builds measured 13.91 and 14.54
seconds with 16 jobs on devbig014 on 2026-08-03; clean CI builds enforce a
30-second ratchet derived from those observations.

The current stripped runtime payload is 8.61 MB apparent size and its
deterministic gzip archive is 3.22 MB. The often-cited 134 MB figure is the full
upstream source checkout, not the runtime payload or the curated source set.
The build-required curated source is 24.68 MB apparent and 5.23 MB as a
deterministic gzip archive; it contains DynamoRIO core, deployment tools, build
support, and only the five extensions this client uses.

After installation, no setup command is needed:

```console
hermit --backend dbt run /bin/true
```

On first use, Hermit locates the helper in `$HERMIT_DIR/bin`, Cargo's bin
directory, or `PATH`. The helper validates an exact package version, protocol,
target, Detcore ABI, and Detcore source/build identity before it touches the
filesystem. It then extracts to a content-addressed release under
`$HERMIT_DIR/plugins/dynamorio` (default `$HOME/.hermit`) while holding an
inter-process lock, validates every file hash and the Detcore DSO descriptor,
and atomically changes `current` only after the release is complete. A newer
helper has a different release key, so an older extracted payload is never
silently reused.

An incompatible helper is a hard error; Hermit does not continue searching and
silently select another copy. An unwritable `$HERMIT_DIR` exits 73 and tells the
user to select a writable install root. `$HERMIT_DIR` is strictly an end-user
install/configuration root. Developer validation history remains under
`$DEV_HERMIT_PARENT` and is not written here.

Prebuilt distribution through operating-system package managers is separate
future work. Cargo remains the source-install channel.
