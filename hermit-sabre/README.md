# hermit-sabre

`cargo install --locked hermit-sabre` builds the pinned SaBRe source and the matching
Detcore shared object, embeds their stripped runtime payload in the helper, and
installs only the `hermit-sabre` executable. Hermit invokes the helper on first
use; it atomically extracts the exact payload under
`$HERMIT_DIR/plugins/sabre` (default `$HOME/.hermit/plugins/sabre`). No setup,
submodule checkout, prebuilt download, or runtime network access is required.

The helper refuses package-version, protocol, target, SaBRe ABI, or Detcore
build-identity skew. CMake, GCC/G++, `strip`, and the SaBRe system development
libraries are source-install prerequisites.
