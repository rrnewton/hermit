# Shared Detcore LiteInst preload

This independent workspace builds `libhermit_liteinst_detcore.so`, linking the
shared `hermit-detcore::Detcore` (whose default type parameter is `NoopTool`).
The artifact builder now targets this DSO, with byte-bound provenance. The
CLI captured and inherited runners select it through a caller-owned common
capture session. Artifact validation and compilation are not runtime qualification.

The product route is `scripts/stage-liteinst-runtime.sh`,
`libhermit_liteinst_detcore.so`, and byte-bound `.provenance.json`. The
artifact validator supports non-overlapping 4 KiB load pages and requires
effective read-only descriptor pages, including the loader's page-rounded
RELRO protection, not merely declared byte coverage.

After the Reverie logging, clock-constructor and subscriber teardown fixes are landed and pinned, from the
repository root use its selected nightly toolchain and a private
external target directory, build independently (under the parent's build
admission protocol on shared hosts):

```sh
CARGO_TARGET_DIR=/absolute/private/external/target cargo build --locked --offline -j2 \
  --manifest-path liteinst-runtime-build/detcore-runtime/Cargo.toml
```

The separate workspace disables Reverie's generic `preload-constructor` feature
to avoid two constructors installing process-global state. The launcher
must explicitly select this constructor with the internal environment handshake
`HERMIT_LITEINST_DETCORE_BOOTSTRAP=hermit-detcore-liteinst-v1` and provide a sealed
bootstrap. Without that environment entry, loading is inert and never scans
`/proc/self/fd`. A present but incorrect selection, missing/unreadable bootstrap,
or invalid installation exits 127 without attempting diagnostics, even when
stderr is closed or blocked. The sealed payload is strict JSON schema 2, with
exactly `version`, `tool`, `config_wire_fingerprint`, and `log_filter`.
The shared codec in `hermit-cli/src/liteinst_bootstrap.rs` rejects unknown fields,
old unversioned payloads, wrong tool/schema/fingerprint and invalid filters.
The environment entry alone cannot activate the tool; the CLI session supplies
both the selector and sealed payload.
The fingerprint checks only config wire-schema compatibility, not code identity
or artifact provenance. Config itself arrives through the coordinator RPC.
Installation explicitly selects Reverie's
`SyscallMode::UserDispatchWithoutPatching`; it does not patch guest syscall
sites. The initializer does not configure runtime signals or bypass the shared
LiteInst capability checks, so unsupported Detcore subscriptions still fail
closed during installation.

The sole registration uses Reverie's `clocked_initializer!` macro, not a second
Rust `.init_array` entry. Its `unsafe extern "C" fn initialize() -> i32` returns
0 when unselected, 1 after installation, and 127 on failure. All bootstrap,
selector and temporary Rust values are dropped before returning to the macro's
assembly finalizer. Only that finalizer enables the published disabled,
thread-owned clock and returns to the loader; Rust never enables it. Unselected
finalization restores prior ownership without PMU, RPC or signal setup. Invalid
state or failure exits 127 through the shared raw exit path, without stderr or
unwinding. Initialization is single-shot before application threads start.
The manifest's current Reverie pin does not contain the split
`reverie-liteinst-runtime` package or the required capture/session APIs, so a
normal locked, offline stage refuses during dependency resolution. Normal
staging remains unavailable until Hermit pins a genuine Reverie commit that
contains both. An external source override is diagnostic-only and cannot make a
normal artifact or installation admissible.

`cargo test --lib` retains the four payload tests and tests the Rust initializer's
return contract in bounded subprocesses (GNU `/usr/bin/timeout` required). These
do not execute the macro's constructor/finalizer; actual DSO load controls must
also verify inert selection and failclosed bootstrap behavior with closed,
broken and blocked stderr against the matching shared implementation.

The migrated source must not be executed before independent source/pair review.
Default Detcore admission still refuses unsupported capabilities. Early loader
coverage, real preemption, process/thread lifecycle and concurrent INFO ordering
remain qualification obligations; no determinism assurance is claimed here.

## Artifact admission

The exported `hermit_liteinst_detcore_descriptor_v1` is a 32-byte C-layout
read-only descriptor: magic `HLI_DSO1`, u32 version 1, u32 size 32, u32 mode 1
(SUD without patching), u32 reserved zero, and the existing clocked constructor
pointer. It does not introduce another constructor or change the assembly tail.
The shared validator in `hermit-cli/src/liteinst_artifact.rs` requires mapped
PT_DYNAMIC constructor/relocation tables and relocated, in-object pointers.
Bare link-time pointers, preemptible symbol relocations, and unsupported packed
relocations refuse; section metadata alone cannot establish registration.

Staging writes `<runtime>.provenance.json`, binding SHA-256 and byte length to
the ABI, declared/resolved Reverie revisions, and verified source-pair digest.
The source record includes both exact-config resolved dependency graphs and
tracked/untracked source identities. The builder checks it before and after
compilation; the CLI build independently checks it before embedding identity.
Changing the record at the same pathname invalidates the build. The CLI's
artifact validator refuses missing identity and mismatched bytes/graphs/mode.
Config's wire fingerprint remains a distinct runtime compatibility check.

Admitted builds provide `HERMIT_LITEINST_SOURCE_RECORD`,
`HERMIT_LITEINST_HERMIT_ROOT`, and `HERMIT_LITEINST_REVERIE_ROOT`.
Optional `HERMIT_LITEINST_CLI_MANIFEST`, `HERMIT_LITEINST_DSO_MANIFEST`, and
`HERMIT_LITEINST_CARGO_CONFIG` select external diagnostic inputs;
the Cargo config is forwarded after every Cargo subcommand.
All graph resolution and nested builds are offline and locked.

For unpublished dirty source use `HERMIT_LITEINST_DIAGNOSTIC=1`, an external
artifact directory, and a CLI compiled against the same verified source record.
Normal resource installation rejects this mode. Do not overwrite published
pins, copy diagnostic artifacts into normal resources, or describe the
declared pin as the actual overridden source. Hashes provide accidental
corruption/staleness checks, not hostile-process or artifact-author attestation.
The pin-only test cache no longer admits same-pin stale DSOs.

Host and guest include `hermit-cli/src/liteinst_record.rs`; the DSO does not link
libhermit. Complete records enter the existing ordered shared transport through
`write_record`; formatter failure calls `record_failed`, never fragmented `Write`.
The host resolves the filter once with the existing permissive environment,
`tokio=debug`, then selected default precedence. Validated directive spelling is
sealed, including float predicates. Both producers use plain canonical formatting.
The CLI installs its subscriber process-globally inside each verification child,
before GlobalTool initialization, and retains RecordStatus and capture ownership
outside the runtime through cleanup and Drop. Guest cancellation does not close
host publication. There is no DetlogForwarder or after-host log append.

The existing log-file ceiling and exact marker remain nonfatal; subsequent bytes
are drained/discarded and evidence is incomplete. Stderr keeps its existing
unlimited destination policy. Transport limits are separate fatal safety bounds:
64 producers, 16 slots each, 1 MiB per record, 8 MiB pending per role, 1024 pending
records, and 64 KiB retained diagnostics. Formatter spans are bounded at 64 KiB.
Startup/blocked publication bounds are 10 seconds and final drain is bounded at
2 seconds independently of the unchanged guest deadline. These are failure bounds,
not determinism tolerances or permission to reset the guest's clock/deadline.
The source order preserves causal publication edges; concurrent-emitter
determinism and cross-backend order have not been established.

Diagnostic builds require an immutable external tracing-subscriber 0.3.23 source
override with the reviewed Writer setters and initial-field-cache hook. There is
no published dependency carrying that change in this product graph. No registry
source, version, or revision pin is replaced here. The added serde/serde_json/
tracing edges use packages already in the dependency graph. This consumer's
manifest is hand-authored; the CLI BUCK source list includes the shared formatter.
