#!/usr/bin/env bash
# Build the pinned validate root from the committed flake.lock, load it into
# podman, and record the resulting digest.
#
# A TOOLCHAIN BUMP IS ONE REVIEWED CHANGE. Edit flake.nix, run this, and commit
# flake.nix + flake.lock + image.digest together. `image.digest.prev` is written
# automatically so the previous root is one edit away for rollback: put the old
# digest back in image.digest, or pass --digest to run-in-pinned-root.sh.
#
# NETWORK. This script fetches; the VALIDATE RUN does not. On a host behind a
# forward proxy the fetch must be authorized for THIS process -- measured on a
# validate host, the proxy is per-identity and a podman container is not an
# authorized client, which is why the image is built on the host with nix rather
# than by running nix inside a container.

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$HERE"

command -v nix >/dev/null 2>&1 || {
    echo "build-image: nix is not on PATH." >&2
    echo "  Single-user install is enough; no daemon is required." >&2
    exit 2
}

NIX=(nix --extra-experimental-features "nix-command flakes")
runner=()
if command -v with-proxy >/dev/null 2>&1; then runner=(with-proxy); fi

echo ":: building image from the committed lock"
out=$("${runner[@]}" "${NIX[@]}" build --no-link --print-out-paths .#image)
echo ":: nix store path: $out"
echo ":: tarball sha256: $(sha256sum "$out" | cut -d' ' -f1)"

echo ":: loading into podman"
loaded=$(podman load -i "$out")
echo ":: $loaded"

# Record the full name@digest reference: `podman image exists` does not resolve
# a bare manifest digest, so a bare sha256 would make the runner fail closed on
# an image that is in fact present. Measured, not assumed.
digest="localhost/hermit-hermetic-validate@$(podman inspect --format '{{.Digest}}' hermit-hermetic-validate:nix)"
[[ -n "$digest" ]] || { echo "build-image: could not read the loaded image digest" >&2; exit 1; }

if [[ -f image.digest ]]; then
    prev=$(tr -d '[:space:]' < image.digest)
    if [[ "$prev" != "$digest" ]]; then
        printf '%s\n' "$prev" > image.digest.prev
        echo ":: previous digest preserved for rollback: $prev -> image.digest.prev"
    fi
fi
printf '%s\n' "$digest" > image.digest
echo ":: image.digest = $digest"
echo
echo "Commit flake.nix, flake.lock and image.digest together."
