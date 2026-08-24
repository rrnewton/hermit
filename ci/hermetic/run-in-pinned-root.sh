#!/usr/bin/env bash
# Run a command inside the nix-pinned validate root. OPT-IN: nothing calls this
# by default, and the ordinary validate path is unchanged.
#
# SHAPE, unchanged from the stage-1 recommendation. The existing outer
# systemd-run, validate-lock and cgroup policy stay exactly as they are; this is
# only the filesystem mechanism, and it deliberately adds no second resource or
# cgroup layer. The payload is one privileged podman container pinned BY DIGEST,
# with /dev/kvm passed through, no runtime network, read-only source at /src,
# and separate writable output and target volumes.
#
# WHY BY DIGEST AND NOT BY TAG. A tag is mutable; a digest is the artifact that
# actually ran. The digest belongs in the receipt next to the flake.lock: the
# digest says what ran, the lock says how to rebuild it. See README.md.
#
#   usage: run-in-pinned-root.sh --src DIR --out DIR [--digest NAME@SHA] -- CMD...

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DIGEST_FILE="$HERE/image.digest"

src=""; out=""; digest=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --src) src=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --digest) digest=$2; shift 2 ;;
        --) shift; break ;;
        *) echo "run-in-pinned-root: unexpected argument '$1'" >&2; exit 2 ;;
    esac
done

[[ -n "$src" ]] || { echo "run-in-pinned-root: --src is required" >&2; exit 2; }
[[ -n "$out" ]] || { echo "run-in-pinned-root: --out is required" >&2; exit 2; }
[[ $# -gt 0 ]] || { echo "run-in-pinned-root: a command is required after --" >&2; exit 2; }

if [[ -z "$digest" ]]; then
    [[ -f "$DIGEST_FILE" ]] || {
        echo "run-in-pinned-root: no --digest and no $DIGEST_FILE." >&2
        echo "  Build the image first: ci/hermetic/build-image.sh" >&2
        exit 2
    }
    digest=$(tr -d '[:space:]' < "$DIGEST_FILE")
fi

# FAIL CLOSED ON A MISSING IMAGE. Falling back to a tag, or to the host, would
# silently produce a run that is not hermetic while still reporting success --
# which is worse than not running at all, because the receipt would claim a
# pinned root it did not use.
if ! podman image exists "$digest"; then
    echo "run-in-pinned-root: image $digest is not present locally." >&2
    echo "  This path does not fall back to a tag or to the host: a run that is" >&2
    echo "  not in the pinned root must not be recorded as if it were." >&2
    echo "  Rebuild it from the committed lock: ci/hermetic/build-image.sh" >&2
    exit 1
fi

mkdir -p "$out/target" "$out/home"

# `--network=none` is the point, not a precaution: if the run can reach the
# network it can pick up something the lock does not describe, and the rebuild
# guarantee is void. CARGO_NET_OFFLINE in the image makes that fail loudly.
exec podman run --rm \
    --privileged \
    --device /dev/kvm \
    --network=none \
    --mount "type=bind,source=$src,destination=/src,ro=true" \
    --mount "type=bind,source=$out/target,destination=/out/target" \
    --mount "type=bind,source=$out/home,destination=/build" \
    -e HOME=/build \
    -e CARGO_HOME=/build/.cargo \
    -e CARGO_TARGET_DIR=/out/target \
    -w /src \
    "$digest" \
    "$@"
