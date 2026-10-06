#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Build immutable inputs consumed by functional strict L2 probes.

set -euo pipefail

if (($# != 1)); then
    echo "usage: $0 FIXTURE_ROOT" >&2
    exit 2
fi

readonly FIXTURE_ROOT=$1
# The receipt the pinned root writes beside the fixtures (see write_receipt).
# Remove it first, so a run that fails, or that runs outside the pinned root,
# leaves no receipt that vouches for the fixtures it rebuilt.
readonly RECEIPT=${FIXTURE_ROOT%/}.receipt
rm -f "$RECEIPT"
rm -rf "$FIXTURE_ROOT"
mkdir -p "$FIXTURE_ROOT/binutils" "$FIXTURE_ROOT/gprof" "$FIXTURE_ROOT/gcov" \
    "$FIXTURE_ROOT/lsof" "$FIXTURE_ROOT/df" "$FIXTURE_ROOT/toolchain"

# Compatibility probes run from an otherwise-empty /test. Snapshot the one
# repository input used by the coreutils rows instead of accidentally relying
# on the validate driver's current directory.
cp "$PWD/README.md" "$FIXTURE_ROOT/README.md"

# `--base-env=minimal` deliberately excludes the user's rustup directory. The
# cargo workload still needs the active toolchain, so expose exactly cargo and
# rustc through this run-owned fixture rather than passing through the host PATH.
# The pinned root has no rustup: its cargo and rustc are the image's own
# toolchain on PATH, so resolve those instead.
toolchain_program() {
    if command -v rustup >/dev/null 2>&1; then
        rustup which "$1"
    else
        command -v "$1"
    fi
}
ln -s "$(toolchain_program cargo)" "$FIXTURE_ROOT/toolchain/cargo"
ln -s "$(toolchain_program rustc)" "$FIXTURE_ROOT/toolchain/rustc"

gcc -O2 -Wall -Wextra -Werror -Wl,--build-id=none \
    "$PWD/tests/compat/localhost_http_server.c" \
    -o "$FIXTURE_ROOT/localhost-http-server"

cat >"$FIXTURE_ROOT/binutils/fixture.c" <<'EOF'
__attribute__((noinline)) int compat_line(int value) {
    return value + 1;
}
EOF
gcc -g -O0 -fno-ident -frandom-seed=hermit-binutils \
    -c "$FIXTURE_ROOT/binutils/fixture.c" \
    -o "$FIXTURE_ROOT/binutils/with-symbols.o"
/bin/readelf -SW "$FIXTURE_ROOT/binutils/with-symbols.o" | grep -q '\.symtab'

cat >"$FIXTURE_ROOT/gprof/profile.c" <<'EOF'
#include <stdio.h>
volatile unsigned long sink;
__attribute__((noinline)) void compat_leaf(unsigned long value) { sink += value; }
__attribute__((noinline)) void compat_root(void) {
    for (unsigned long index = 0; index < 1000000; ++index) compat_leaf(index & 7);
}
int main(void) { compat_root(); printf("%lu\n", sink); return 0; }
EOF
gcc -O0 -pg -fno-pie -no-pie -Wl,--build-id=none \
    "$FIXTURE_ROOT/gprof/profile.c" -o "$FIXTURE_ROOT/gprof/program"
(
    cd "$FIXTURE_ROOT/gprof"
    ./program >program.out
)
test -s "$FIXTURE_ROOT/gprof/gmon.out"

cat >"$FIXTURE_ROOT/gcov/coverage.c" <<'EOF'
#include <stdio.h>
int main(void) {
    int total = 0; /* compat_marker */
    for (int index = 0; index < 5; ++index) {
        if (index % 2 == 0) total += index;
    }
    printf("%d\n", total);
    return total != 6;
}
EOF
(
    cd "$FIXTURE_ROOT/gcov"
    gcc --coverage -O0 -fno-ident -frandom-seed=hermit-gcov \
        -Wl,--build-id=none coverage.c -o coverage
    ./coverage >program.out
)
test -s "$FIXTURE_ROOT/gcov/coverage.gcno"
test -s "$FIXTURE_ROOT/gcov/coverage.gcda"

# lsof unconditionally walks /proc/mounts before applying its PID/FD filters.
# Serve that read from an inherited descriptor for a fixed, valid mount table
# so other users creating or removing host mounts cannot change the
# strict-verify syscall stream.  The preload library refuses a descriptor on
# procfs, so pointing it back at the live table cannot satisfy the marker.
gcc -shared -fPIC -Wall -Wextra -Werror \
    "$PWD/tests/compat/lsof_mount_redirect.c" -ldl \
    -o "$FIXTURE_ROOT/lsof/libmount_redirect.so"
cat >"$FIXTURE_ROOT/lsof/mounts" <<'EOF'
fixture /tmp fixture rw 0 0
EOF

# `df` consults mountinfo before statfs(2). Hermit's private mount roots differ
# between the two executions inside --verify, so give this command-compatibility
# probe a fixed, valid view of the root mount. The product-level mountinfo
# determinization remains covered separately; this fixture does not weaken or
# filter the verifier.
cat >"$FIXTURE_ROOT/df/mountinfo" <<'EOF'
1 0 0:1 / / rw,relatime - rootfs rootfs rw
EOF

# Prevent host build time from becoming guest-visible input.
find "$FIXTURE_ROOT" -exec touch -h -d @1 {} +

# Bash runs a command substitution without errexit, so a failure is returned
# explicitly for the caller's assignment to stop the script.
sha256_of() {
    local digest
    digest=$(sha256sum <"$1") || return 1
    printf '%s' "${digest%% *}"
}

# What built these fixtures and what they hold, for test-harness: before a
# guest_root: pinned bucket runs a cell that reads them, it requires the image
# to be the pinned one, the compiler to be the image's gcc, the sources to be
# the checkout's, and the fixtures to hold exactly the entries below
# (verify_fixture_receipt in ci/manifest-plan/src/compat_guest_root.rs). A
# fixture tree built on the host, or by another checkout, has no receipt that
# matches (https://github.com/rrnewton/hermit/issues/3712). Each entry line
# is KIND<TAB>PATH<TAB>VALUE, with PATH relative to the fixture root and the
# kinds of the `tree:` digest in ci/hermetic/image-paths.txt: x and f for a
# regular file with and without an execute bit, by the SHA-256 of its bytes,
# and l for a symbolic link, by its target. Each value is assigned before it
# is printed, so a failed command stops the script instead of printing an
# empty field.
write_receipt() {
    local entries compiler digest kind mode rel path source target
    entries=$(find "$FIXTURE_ROOT" -mindepth 1 -printf '%y %m %P\n' | LC_ALL=C sort -k 3)
    compiler=$(command -v gcc)
    compiler=$(readlink -f "$compiler")
    printf 'hermit-compat-fixture-receipt 1\n'
    printf 'image %s\n' "$HERMIT_PINNED_ROOT_IMAGE"
    printf 'compiler %s\n' "$compiler"
    digest=$(sha256_of "${BASH_SOURCE[0]}")
    printf 'source %s %s\n' "$digest" tests/compat/prepare_real_compat_fixtures.sh
    for source in tests/compat/localhost_http_server.c tests/compat/lsof_mount_redirect.c README.md; do
        digest=$(sha256_of "$PWD/$source")
        printf 'source %s %s\n' "$digest" "$source"
    done
    while read -r kind mode rel; do
        path=$FIXTURE_ROOT/$rel
        case $kind in
            d) ;;
            l)
                target=$(readlink "$path")
                printf 'l\t%s\t%s\n' "$rel" "$target"
                ;;
            f)
                ((8#$mode & 8#111)) && kind=x
                digest=$(sha256_of "$path")
                printf '%s\t%s\t%s\n' "$kind" "$rel" "$digest"
                ;;
            *)
                printf 'cannot record %s: not a file, a link or a directory\n' "$path" >&2
                return 1
                ;;
        esac
    done <<<"$entries"
}

# ci/hermetic/run-in-pinned-root.sh sets HERMIT_PINNED_ROOT_IMAGE; the host
# and the other fixture copies get no receipt.
if [[ -n ${HERMIT_PINNED_ROOT_IMAGE:-} ]]; then
    write_receipt >"$RECEIPT.tmp.$$"
    mv "$RECEIPT.tmp.$$" "$RECEIPT"
fi
printf 'prepared real compatibility fixtures: %s\n' "$FIXTURE_ROOT"
