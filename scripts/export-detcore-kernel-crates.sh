#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.
#
# export-detcore-kernel-crates.sh
# -------------------------------
# Export Detcore's crates, as the Narf kernel builds them without std, to a
# directory that can be committed on its own and used as a Cargo git
# dependency.
#
# usage: scripts/export-detcore-kernel-crates.sh --reverie-rev SHA --out DIR
#
# Why: a Cargo git dependency on this repository would check out its
# submodules, and its detcore/Cargo.toml and detcore-model/Cargo.toml, which
# autocargo generates, describe the host build. The export has no submodules
# and carries its own manifests for those two packages, next to the unchanged
# sources.
#
# What, from the committed HEAD of this checkout (never the working tree):
#   detcore/src, detcore-model/src    the sources, unchanged
#   detcore-std, detcore-clap         unchanged
#   detcore-libc                      unchanged but for its Reverie rev, SHA
#   kernel/hermit-detcore/Cargo.toml  the manifests of check-detcore-nostd.sh
#   kernel/detcore-model/Cargo.toml   (scripts/detcore-nostd-manifests.sh),
#                                     with Reverie at SHA
#   rand_pcg-0.10.2                   the crates.io package, its checksum
#                                     checked against Cargo.lock, with serde's
#                                     default features off, as
#                                     check-detcore-nostd.sh patches it
#   LICENSE, README.md                the license, and where the export came
#                                     from
#
# SHA is a full commit of https://github.com/rrnewton/reverie. Every crate of
# the consumer's build must take Reverie from that same commit, or Cargo builds
# two reverie-core crates whose Tool traits differ. The consumer depends on
# hermit-detcore from the export's commit and patches crates.io's rand_pcg to
# the same commit.
#
# DIR must not exist. To publish, commit DIR on its own, as a commit with no
# parent, and push that commit to a branch.
#
# Exit codes:
#   0  exported
#   2  usage / environment error

set -euo pipefail

usage() {
    echo "usage: $0 --reverie-rev SHA --out DIR"
}

rev=""
out=""
while (($# > 0)); do
    case "$1" in
        --reverie-rev | --out)
            if (($# < 2)); then
                echo "error: $1 requires a value" >&2
                exit 2
            fi
            if [[ $1 == --reverie-rev ]]; then rev=$2; else out=$2; fi
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ ! $rev =~ ^[0-9a-f]{40}$ ]]; then
    echo "error: --reverie-rev must be a full 40-hex commit" >&2
    usage >&2
    exit 2
fi
if [[ -z $out ]]; then
    echo "error: --out DIR is required" >&2
    usage >&2
    exit 2
fi
if [[ -e $out ]]; then
    echo "error: $out exists" >&2
    exit 2
fi

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
head="$(git -C "$REPO_ROOT" rev-parse --verify HEAD)"
# The README says the export came from this script at HEAD; make it true.
if ! git -C "$REPO_ROOT" diff --quiet HEAD -- scripts/export-detcore-kernel-crates.sh \
    scripts/detcore-nostd-manifests.sh; then
    echo "error: commit scripts/export-detcore-kernel-crates.sh and" \
        "scripts/detcore-nostd-manifests.sh first" >&2
    exit 2
fi
# shellcheck source=scripts/detcore-nostd-manifests.sh
source "$REPO_ROOT/scripts/detcore-nostd-manifests.sh"

# rand_pcg 0.10.2 from the cargo cache, as Cargo.lock at HEAD pins it.
pcg=rand_pcg-0.10.2
want="$(git -C "$REPO_ROOT" show "$head:Cargo.lock" |
    awk -v RS= '/\nname = "rand_pcg"\nversion = "0\.10\.2"\n/' |
    sed -n -E 's/^checksum = "([0-9a-f]{64})"$/\1/p')"
if [[ ! $want =~ ^[0-9a-f]{64}$ ]]; then
    echo "error: Cargo.lock at $head has no single checksum for $pcg" >&2
    exit 2
fi
crate=""
for f in "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/"$pcg.crate"; do
    if [[ -f $f && $(sha256sum "$f" | cut -d ' ' -f 1) == "$want" ]]; then
        crate=$f
        break
    fi
done
if [[ -z $crate ]]; then
    echo "error: no $pcg.crate with Cargo.lock's checksum in the cargo cache;" \
        "run cargo fetch in $REPO_ROOT" >&2
    exit 2
fi

mkdir -p "$out"
git -C "$REPO_ROOT" archive --format=tar "$head" -- detcore/src detcore-model/src \
    detcore-std detcore-libc detcore-clap LICENSE | tar -xf - -C "$out"

libc_toml="$out/detcore-libc/Cargo.toml"
if [[ $(grep -c -E 'rev = "[0-9a-f]{40}"' "$libc_toml") -ne 1 ]]; then
    echo "error: $libc_toml does not pin exactly one rev; update this script" >&2
    exit 2
fi
sed -i -E "s/rev = \"[0-9a-f]{40}\"/rev = \"$rev\"/" "$libc_toml"

tar -xzf "$crate" -C "$out"
pcg_toml="$out/$pcg/Cargo.toml"
sed -i '/^\[dependencies\.serde\]$/a default-features = false' "$pcg_toml"
if [[ $(grep -c -x '\[dependencies\.serde\]' "$pcg_toml") -ne 1 ]] ||
    [[ $(grep -A 1 -x '\[dependencies\.serde\]' "$pcg_toml" | tail -n 1) != 'default-features = false' ]]; then
    echo "error: $pcg's manifest is not the expected one; update this script" >&2
    exit 2
fi

reverie_git="git = \"https://github.com/rrnewton/reverie.git\", rev = \"$rev\""
header="# Generated by scripts/export-detcore-kernel-crates.sh from Hermit commit
# $head. See that script and scripts/detcore-nostd-manifests.sh."
mkdir -p "$out/kernel/hermit-detcore" "$out/kernel/detcore-model"
{
    echo "$header"
    detcore_nostd_manifest ../../detcore/src/lib.rs ../../detcore-libc ../detcore-model \
        ../../detcore-std "$reverie_git"
} >"$out/kernel/hermit-detcore/Cargo.toml"
{
    echo "$header"
    detcore_model_nostd_manifest ../../detcore-model/src/lib.rs ../../detcore-clap \
        ../../detcore-libc ../../detcore-std "$reverie_git"
} >"$out/kernel/detcore-model/Cargo.toml"

cat >"$out/README.md" <<EOF
# Detcore for the Narf kernel

Generated; do not edit. This is Detcore, the deterministic execution engine of
Hermit (https://github.com/facebookexperimental/hermit), packaged for the Narf
kernel, which builds it without std.

- Hermit commit: $head
- Reverie commit: $rev (https://github.com/rrnewton/reverie)
- rand_pcg 0.10.2: the crates.io package (sha256 $want) with
  \`default-features = false\` added to its serde dependency

The sources are Hermit's, unchanged, except for the Reverie rev in
detcore-libc/Cargo.toml. The manifests under kernel/ replace Hermit's
generated detcore/Cargo.toml and detcore-model/Cargo.toml, which describe the
host build.

To depend on it, take \`hermit-detcore\` from this commit and patch crates.io's
\`rand_pcg\` to this commit, and take every Reverie crate from the Reverie
commit above.

To regenerate, in a Hermit checkout at the commit above:

    scripts/export-detcore-kernel-crates.sh --reverie-rev $rev --out DIR
EOF

echo "exported Hermit $head with Reverie $rev to $out"
