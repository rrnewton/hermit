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
# FETCH WITH NETWORK, THEN BUILD AND TEST WITHOUT. Owner ruling, 2026-08-24.
#
# The problem this solves, measured rather than predicted: with a cold
# CARGO_HOME the offline run below fails in ONE SECOND --
#   error: failed to get `reverie-core` as a dependency of package `hermit-detcore`
#   can't checkout from '...': you are in the offline mode (--offline)
# Cargo.lock names 310 packages, 20 of them git-sourced, and `--network=none`
# plus CARGO_NET_OFFLINE makes every one unreachable. So the pinned root pinned
# the OS and the toolchain but NOT the Rust dependency sources: it only ever
# worked on a machine whose ~/.cargo already happened to hold them, and a fresh
# runner failed immediately.
#
# `--fetch` adds one preparatory phase that is allowed the network and does
# nothing else: `cargo fetch --locked` populates $out/home/.cargo, and `--locked`
# means Cargo.lock decides what is fetched, not ambient state. The build and the
# tests still run with `--network=none`, so nothing the lock does not describe
# can influence a result. The network is used to OBTAIN pinned inputs, never
# while producing an output.
#
# WHY THE FETCH RUNS ON THE HOST AND NOT IN A NETWORKED CONTAINER. Measured on
# devbig014: from inside the image, with --network=host and every proxy variable
# set, `git ls-remote https://github.com/rrnewton/reverie.git` fails with
# "Recv failure: Connection reset by peer", while the identical command on the
# host through the proxy succeeds. Container egress is blocked here. The fetch
# is therefore a host step. That costs nothing in hermeticity -- its output is
# determined by Cargo.lock and it is verified offline afterwards -- and it keeps
# stage 3 usable on this box. On a runner with container egress the same phase
# can move inside; the phase boundary is what matters, not where it runs.
#
#   usage: run-in-pinned-root.sh --src DIR --out DIR [--digest NAME@SHA]
#                                [--fetch] -- CMD...

set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
DIGEST_FILE="$HERE/image.digest"

src=""; out=""; digest=""; fetch=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --src) src=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        --digest) digest=$2; shift 2 ;;
        --fetch) fetch=1; shift ;;
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

# PHASE 1, OPT-IN AND SEPARATE: obtain the pinned sources, produce nothing.
if (( fetch )); then
    echo ":: fetch phase (network allowed; Cargo.lock decides what is fetched)"
    if ! command -v cargo >/dev/null 2>&1; then
        echo "run-in-pinned-root: --fetch needs cargo on the host PATH" >&2
        exit 2
    fi
    # `with-proxy` where the host requires it, plain cargo where it does not.
    # Absence of the wrapper is not an error: a host with direct egress needs it
    # not, and a host with neither will fail loudly in cargo rather than here.
    proxy_wrapper=()
    command -v with-proxy >/dev/null 2>&1 && proxy_wrapper=(with-proxy)
    if ! CARGO_HOME="$out/home/.cargo" CARGO_NET_OFFLINE=false \
            "${proxy_wrapper[@]}" cargo fetch --locked --manifest-path "$src/Cargo.toml"; then
        echo "run-in-pinned-root: fetch phase FAILED; not proceeding to the offline run." >&2
        echo "  The offline run would fail on the same dependencies, one second in," >&2
        echo "  and that failure would be harder to read than this one." >&2
        exit 1
    fi
    # PINNED DEVELOPER TOOLS BELONG IN THE FETCH PHASE TOO, for the same reason
    # the crates do. `setup.nextest` wants EXACTLY 0.9.100 and installs it from
    # the network when absent; offline it fails with "could not find
    # `cargo-nextest` in registry `crates-io` with version `=0.9.100`". Shipping
    # the version in the pinned nixpkgs instead -- 0.9.72 -- WOULD make the node
    # pass, because it only probes `cargo nextest show-config version`, and that
    # is precisely why it is the wrong fix: the node would go green while the
    # tests ran under a version the project did not pin. Installing the exact
    # version here keeps the pin honest and keeps the run offline.
    nextest_version=$(sed -n 's/.*cargo install cargo-nextest --locked --version \([0-9.]*\).*/\1/p' \
        "$src/ci/dag/portable.json" | head -n1)
    if [[ -n "$nextest_version" ]]; then
        # INSTALL TO A THROWAWAY --root, NEVER INTO THE SHARED CARGO_HOME/bin.
        # A host-built binary CANNOT RUN in the nix root -- its ELF interpreter
        # does not exist there -- and installing it into $CARGO_HOME/bin also
        # records it in .crates.toml, which made `setup.nextest` exit 0 on
        # "already installed" while every one of the 15 downstream test nodes
        # died on "could not execute process .../cargo-nextest (never
        # executed)". A gate reporting a tool present when it is unrunnable is
        # worse than the missing tool. What this phase is FOR is the crate
        # SOURCE in the shared registry cache; the in-image `cargo install` then
        # builds a runnable binary offline from it.
        if ! CARGO_HOME="$out/home/.cargo" CARGO_NET_OFFLINE=false \
                "${proxy_wrapper[@]}" cargo install cargo-nextest --locked \
                --version "$nextest_version" --root "$out/home/.nextest-fetch" \
                >/dev/null 2>&1; then
            echo "run-in-pinned-root: WARNING: could not install cargo-nextest $nextest_version;" >&2
            echo "  setup.nextest will fail offline. Not fatal: nodes that do not use it still run." >&2
        else
            echo ":: fetch phase cached cargo-nextest $nextest_version source (built in-image)"
        fi
    fi
    echo ":: fetch phase complete; the run below has no network"
fi

# `--network=none` is the point, not a precaution: if the run can reach the
# network it can pick up something the lock does not describe, and the rebuild
# guarantee is void. CARGO_NET_OFFLINE in the image makes that fail loudly.
exec podman run --rm \
    --privileged \
    --device /dev/kvm \
    --network=none \
    --mount "type=bind,source=$src,destination=/src,ro=true" \
    --mount "type=bind,source=$out/target,destination=/out/target" \
    `# AND THE SAME VOLUME AT /src/target. CARGO_TARGET_DIR redirects CARGO,` \
    `# but it does not redirect the project. Measured over all 53 portable DAG` \
    `# nodes: 20 died on "mkdir: cannot create directory 'target': Read-only` \
    `# file system" and 14 more downstream on a missing /src/target -- because` \
    `# the node commands reference the literal path (target/debug/test-harness,` \
    `# target/install_pkg/rsrcs/..., and plain mkdir target). /src stays` \
    `# read-only; this makes exactly the one subpath that must be writable so,` \
    `# and points it at the same volume cargo already writes to, so the two` \
    `# views cannot diverge.` \
    --mount "type=bind,source=$out/target,destination=/src/target" \
    --mount "type=bind,source=$out/home,destination=/build" \
    -e HOME=/build \
    -e CARGO_HOME=/build/.cargo \
    -e CARGO_TARGET_DIR=/out/target \
    -w /src \
    "$digest" \
    "$@"
