# Native package actions

These host entries use Reverie's native action authority. They are intentionally
outside `src/bin`: the Cargo pin does not expose the native API. The native Buck
graph must compile and run these actual entries and their tests against the
coordinated signed Reverie successor. Cargo compilation does not test this path.
Integration currently awaits the corrected signed R owner-pin/source-table API
and actual native graph validation; the frozen API proposal is not qualification.

`stage_detcore.rs` takes `--expected JSON --declared JSON --resources DIR
--stage NEW_DIR --logs NEW_DIR`. ExpectedGraph is independently authored;
DeclaredGraph comes from normal typed producer associations. Records and hashes
are data until `qualify_declared_constructor` freshly checks the actual
source/producer graph and physical ordinary constructor. The corrected R API
requires an independent `ExpectedProducer.owner_pin` (exact owner SHA256 and
size) for each retained producer, including every dependency, consumed by a
different executable. The original
producing action's manifest must contain the same pin; H cannot override an old
manifest or substitute its own executable for the original owner.

The ordinary action copies the qualified runtime, checks installed bytes,
rebinds, and publishes `libdetcore_liteinst.native-package.json` last. The stage
must be an immediate child of the resources directory. Publication atomically
refuses replacement. Success removes the empty owned stage; failure removes
only outputs still owned by this invocation.

`stage_native_exec.rs` takes `--expected JSON --declared JSON --bundle-expected
JSON --bundle-declared JSON --stage NEW_DIR --logs NEW_DIR`. Bootstrap and runner
are normal declared Buck C/assembly/link products with explicit source,
toolchain/support and environment associations. Native-action owns these rules
and preserves the H0956 recipe. H executes no compiler or linker for these roles.
It copies their actual Artifacts into the protected stage, checks those copies
against the selected pins, and applies the existing physical finalizer.

Both entries directly depend on `reverie-liteinst-artifact`, `serde`,
`serde_json`, and `libc`. The explicit entry additionally needs the matching
native `detcore-model` and `reverie-inguest` source-digest dependencies, `goblin`,
and `sha2`, with `--cfg 'feature="native-exec"'` in the real rustc action. Its
native test target must use the same source, dependencies and cfg. The entry
uses `with_declared_native_entry`; the publication guard remains armed through
that factory's unconditional final custody close. Ordinary constructor evidence
cannot admit this path. Both entries reuse Cargo's same
`native_bundle.rs`/`protected_copy.rs` publication implementation.

The bundle records are separate from the original R runtime producer graph:
the runner links the runtime, so it cannot be an input to that runtime's original
producer. Each bundle record has exactly these eleven keys:

- `bootstrap`, `runner`, and `runner_runtime`;
- `rv/bootstrap/private-bootstrap.c`, `rv/bootstrap/private-runner.c`,
  `rv/bootstrap/private-control.h`, `rv/bootstrap/private-entry.S`,
  `rv/bootstrap/private-fs.S`, `rv/bootstrap/private-kernel-state.c`,
  `rv/bootstrap/private-maps.c`, `rv/bootstrap/private-bootstrap.ld`.

Each of the eleven expected-map values has the exact shape
`{"sha256": "<64 lowercase hex>", "size": <bytes>}`. The eight source expectations
are independently authored from the selected sources. The generated `bootstrap`
and `runner` pins are stability observations captured from the actual typed
product providers after their normal prerequisite actions finish and before the
H consumer starts. They bind the selected product bytes at open and close; they
do not independently establish source or compilation provenance. The generated
`runner_runtime` pin is also a stability observation. H additionally requires
that its actual file identity equal the freshly R-qualified explicit-entry
runtime. No generated-product observation is labelled independent source
expectation or physical qualification.

The declared map gives each key its current Artifact path through normal
attrs/`$(location)`. The eight source Artifacts must come from the actual selected
bootstrap/runner producers' input associations. `runner_runtime` must be the
actual runtime Artifact used by the runner producer's `-ldetcore_liteinst` link.
Selecting unrelated matching sources or a free-floating library search path
does not meet this graph contract. Both maps require all eleven exact keys;
every expected pin requires SHA256 and size. Both maps are strictly parsed and
bounded and are retained by byte identity. They contain data; matching hashes
do not create source or physical qualification.

At open and again at source close, H resolves every original declared path and
checks its bytes against the selected pin. It compares all eight actual source
files to the exact per-name bytes from
`reverie_inguest::native_exec_sources::bootstrap_sources()`, the same table
consumed by the compiled R source digest. H copies the selected bootstrap and
runner into its protected stage, verifies each copy's SHA256 and size against
the selected pin, and gives those owned copies to the shared finalizer for
physical validation and publication. Original declared paths are still
rechecked at close and must retain their initial identities. The existing
ELF/loader/runtime/full-gate/source/copy checks remain. The normal Buck producers
own the source-to-C-product and compiler-input associations; the H receipt makes
no independent compiler-closure claim. The runtime receives fresh opaque R entry
qualification, never authority reconstructed from records.

Native-action must preserve and inspect the actual normal H0956 build recipe:
the runner uses `-fPIE -pie -Wl,-z,now,-z,relro` and links the selected runtime;
the fixed non-PIE bootstrap uses the existing linker script and `GATE_PAGE`
definition. H's shared producer finalizer does not separately prove the runner's
ET_DYN type, bind-now flags, GNU_RELRO segment or runtime DT_NEEDED entry, nor the
bootstrap's ET_EXEC type and absence of PT_INTERP. Those producer properties
still depend on the exact normal Buck recipe. The consumer retains its existing
image-kind and stack checks when adopting the bundle; later adoption is not a
new H producer ELF proof. Inspection of the actual normal Buck recipes and input
associations is mandatory before any integration claim; product-pin equality
alone does not satisfy it.

The explicit action emits the unchanged eight `native_exec_*` files into the
stage's parent (the consuming compile action's OUT_DIR). Three `.bin` files hold
bootstrap, runner, and runtime bytes. Their SHA256 files and the source and gate
SHA256 files contain raw 32-byte digests. The qualification receipt remains
inside the owned stage and is written after the complete source/custody close.
No Cargo rerun hint is synthesized by the native action.

At build time, run the native installer with its actual build inputs, then run:

```sh
python3 ci/verify-native-packaged-detcore.py INSTALL
```

The same reader checks a copied or relocated installation. It opens only
`INSTALL/rsrcs/libdetcore_liteinst.so` and its native receipt. Historical source,
Buck output, command and log paths are retained evidence; they need not exist in
the installed package. No compiler or build checkout is needed to consume the
package. The reader checks receipt structure and installed bytes; it does not
re-establish source/action authority or perform physical qualification. The
packaging system's trusted-build/copy boundary is unchanged.

Existing Cargo `stage-detcore-liteinst` and `verify-packaged-detcore.py` retain
the distinct `hermit-packaged-detcore/v1` receipt, genuine Cargo fields, and SaBRe
revision checks. Native package actions must select the native reader explicitly;
mixed-runtime Cargo tests still consume Cargo metadata. Standalone preload and
private-static archive/member/final-link qualification are separate obligations.
Neither entry here authorizes private-static inputs.
