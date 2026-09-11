# Private LiteInst native component

Owned bootstrap, private-stack builder, CRT adapter and lifecycle sources for the
SUD-only shared Detcore runtime. Production compilation is invoked by
`../private_build.rs` from the existing runtime-build build script. Every consumer
uses the single `bootstrap/abi-01/private_crt_context.h` and the same current
`bootstrap/src/mapper.h`. Tests are ordinary byte/ownership models; some historical
mapper controls require mapping effects and must not be run without appropriate
authorization.

No GNU checkout or native binary is vendored here. GNU static-PIE libc, renamed
private rcrt1, GCC/assembler/linker and native libraries are explicit external
prerequisites, described by a schema-1 JSON manifest:

```text
{"schema":1,"files":{"cc":{"path":"/absolute/compiler","sha256":"..."},...},"headers":{"gcc":{"path":"/absolute/gcc-headers","sha256":"..."},"system":{"path":"/absolute/system-headers","sha256":"..."}}}
```

The exact required roles are `liteinst_artifact::private::INPUT_FILES`. Use real
artifacts from the approved reproducible GNU/tool recipe, not substitute startup
objects. Each file is hashed before and after the build and its role/content
identity enters the same source record verified by the CLI. This is a content
identity contract, not a claim of public availability or native qualification.
Header identities hash the compact JSON produced by `private::header_tree`:
sorted relative names mapped to SHA256 and byte size. Header trees must be bounded
regular files/directories without symlinks. The producer copies and rechecks both
trees, then compiles with `-nostdinc` using only these copies and product headers.

To stage through the product producer, set `HERMIT_LITEINST_RUNTIME_KIND=private-crt`,
`HERMIT_LITEINST_PRIVATE_INPUTS` to that external manifest,
`HERMIT_LITEINST_HERMIT_ROOT` to the actual Hermit source root,
`HERMIT_LITEINST_REVERIE_ROOT` to the actual resolved source root, and
`HERMIT_LITEINST_SOURCE_RECORD` to an external record path. Its parent must exist;
the producer captures a missing record from real Git source identities and locked
dependency graphs, or verifies an existing record rather than replacing it. Then
invoke `scripts/stage-liteinst-runtime.sh <profile> <destination.elf> <target-root>`.
Build the CLI with the same environment and generated source record after staging.
The CLI verifies the record again before embedding its identity; an unrelated
prebuilt CLI cannot consume the new pair merely because it is adjacent.

Normal installation uses the existing `hermit-install` hook and the distinct
`hermit_liteinst_detcore_private.elf` resource name. Private artifacts are selected
before preload artifacts and never fall back to preload on validation failure.
Diagnostics additionally require `HERMIT_LITEINST_DIAGNOSTIC=1` and local Cargo
overrides; they stay outside product trees and cannot be installed as normal
resources. An old declared Reverie pin is publication debt, not compatibility.
Source hashes and static checks do not authorize private execution or bypass
full-Tool admission. The private kind does not enable patching.
