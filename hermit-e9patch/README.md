# hermit-e9patch

`cargo install --locked hermit-e9patch` builds the pinned e9patch source, embeds the
stripped `e9tool` and `e9patch` runtime in the helper, and installs only the
`hermit-e9patch` executable. Hermit invokes it on first use; it atomically
extracts the exact payload under `$HERMIT_DIR/plugins/e9patch` (default
`$HOME/.hermit/plugins/e9patch`). No setup, submodule checkout, prebuilt
download, or runtime network access is required.

The helper refuses package-version, protocol, target, e9patch handoff, or
Detcore build-identity skew. `make`, GCC/G++, `strip`, and `xxd` are
source-install prerequisites. This package supplies Hermit's current
preprocessor-plus-ptrace adapter; it does not claim a standalone Detcore
e9patch runtime.
