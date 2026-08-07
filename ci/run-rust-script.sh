#!/usr/bin/env bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Run one of Hermit's standalone `scripts/*.rs` / `ci/*.rs` programs by
# COMPILING it with the installed Rust toolchain, never by dispatching through
# its `#!/usr/bin/env rust-script` shebang.
#
# WHY THIS EXISTS. GitHub's portable images provide `rustc` and `cargo` but
# intentionally do NOT install `rust-script`. A CI step that invokes such a
# script through its shebang therefore dies at `execve` with
#
#     /usr/bin/env: 'rust-script': No such file or directory
#
# and exit 127, in about a tenth of a second, having run nothing. That is a
# NO-RESULT, but it reaches the reader wearing the same red badge as a genuine
# test failure. Measured instance: ci-portable run 31224834809, job
# `test: strict-compat` (93017794290) step 14, which had just started reaching
# `scripts/validate.rs` through the `validate.sh` shim.
#
# `ci/run-reverie-pin-check.sh` established the compile-instead-of-interpret
# answer for a single dependency-free script. This is the same idea generalised,
# because the class is eleven executable `.rs` files and FOUR of them declare
# `//! ```cargo` dependencies that bare `rustc` cannot satisfy.
#
# THE SHEBANGS STAY. They are a developer convenience — `./scripts/validate.rs`
# is a pleasant thing to type on a machine that has `rust-script`. What must not
# happen is a *tracked CI caller* depending on that interpreter. Every such
# caller goes through this launcher, so the binary CI runs is built the same way
# a developer's is, from the same source, with no second execution path.
#
# CACHE CORRECTNESS. `rust-script` decides a cached binary is fresh from the
# main script's mtime alone and never inspects `#[path]`-included modules — the
# trap documented at length in `scripts/lib/rust_script_prelude.rs`. This
# launcher keys its cache on the CONTENT of the script, every module it
# transitively includes, its dependency block, and the compiler version, so
# editing an included module rebuilds. Stale-binary-after-edit is not
# representable here.

set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# Default to the edition every existing in-tree rustc launcher already compiles
# with (`ci/run-reverie-pin-check.sh`, `.github/workflows/ci-portable.yml`,
# `.github/workflows/runner-health.yml`, `ci/dag/portable.json`). A script may
# override it with an `edition = "…"` line in its `//! ```cargo` block.
readonly DEFAULT_EDITION=2021

function die {
    echo "ci/run-rust-script.sh: $*" >&2
    exit 2
}

function usage {
    cat >&2 <<'EOF'
Usage: ci/run-rust-script.sh [--self-test] [--print-binary] <script.rs> [args...]

Compiles <script.rs> with the installed Rust toolchain and runs it, forwarding
every remaining argument untouched. Launcher options must precede <script.rs>;
everything after it belongs to the script.

  --self-test      Compile with `--test` and run the script's unit tests.
  --print-binary   Build, print the absolute binary path, and exit without
                   running it.
EOF
    exit 2
}

self_test=0
print_binary=0
while (($#)); do
    case "$1" in
        --self-test)
            self_test=1
            shift
            ;;
        --print-binary)
            print_binary=1
            shift
            ;;
        -h | --help) usage ;;
        --)
            shift
            break
            ;;
        -*) die "unknown launcher option '$1' (launcher options must precede the script path)" ;;
        *) break ;;
    esac
done

(($#)) || usage
script_arg=$1
shift

[[ -f $script_arg ]] || die "no such script: $script_arg"
script=$(cd -- "$(dirname -- "$script_arg")" && pwd)/$(basename -- "$script_arg")
script_dir=$(dirname -- "$script")
script_stem=$(basename -- "$script" .rs)
# Cargo package names may not contain a '.'; the stems in tree use '-' and '_'.
pkg_name=${script_stem//[^A-Za-z0-9_-]/-}

# ---------------------------------------------------------------------------
# Toolchain. Fail LOUDLY and specifically: the whole point of this launcher is
# that a missing interpreter must never again be mistaken for a test result.
# ---------------------------------------------------------------------------
if ! command -v rustc >/dev/null 2>&1; then
    cat >&2 <<EOF
ci/run-rust-script.sh: LAUNCH FAULT — no Rust compiler on PATH.

  script : $script
  need   : rustc (and cargo, if the script declares dependencies)
  PATH   : $PATH

This is a MISSING-TOOLCHAIN fault, not a test result. Nothing was executed, so
no conclusion about $script_stem may be drawn from this exit.
EOF
    exit 2
fi

# ---------------------------------------------------------------------------
# Dependency block. rust-script embeds a Cargo manifest fragment in the leading
# `//!` doc comment, fenced as ```cargo … ```.
# ---------------------------------------------------------------------------
manifest_fragment=$(
    awk '
        /^\/\/! *```cargo/ { inblock = 1; next }
        inblock && /^\/\/! *```[[:space:]]*$/ { exit }
        inblock { sub(/^\/\/![ ]?/, ""); print }
    ' "$script"
)

edition=$DEFAULT_EDITION
if [[ $manifest_fragment =~ edition[[:space:]]*=[[:space:]]*\"([0-9]+)\" ]]; then
    edition=${BASH_REMATCH[1]}
fi

# `[dependencies]` entries may carry `path = "…"` relative to the SCRIPT, which
# is how rust-script resolves them. The generated package lives elsewhere, so
# every relative path is rewritten to an absolute one.
dependencies=$(
    awk -v dir="$script_dir" '
        /^\[/ { section = $0 }
        {
            line = $0
            if (section ~ /dependencies/ && line ~ /path[[:space:]]*=[[:space:]]*"[^\/"]/) {
                match(line, /path[[:space:]]*=[[:space:]]*"[^"]*"/)
                spec = substr(line, RSTART, RLENGTH)
                match(spec, /"[^"]*"/)
                rel = substr(spec, RSTART + 1, RLENGTH - 2)
                sub(/path[[:space:]]*=[[:space:]]*"[^"]*"/, "path = \"" dir "/" rel "\"", line)
            }
            print line
        }
    ' <<<"$manifest_fragment"
)

# ---------------------------------------------------------------------------
# Cache key: script content + every transitively `#[path]`-included module +
# the dependency block + the compiler identity + the compile mode. Content, not
# mtime — see the header note on the rust-script cache trap.
# ---------------------------------------------------------------------------
declare -a sources=("$script")
declare -A seen=(["$script"]=1)
queue=("$script")
while ((${#queue[@]})); do
    current=${queue[0]}
    queue=("${queue[@]:1}")
    current_dir=$(dirname -- "$current")
    while IFS= read -r rel; do
        [[ -n $rel ]] || continue
        included=$(cd -- "$current_dir" && cd -- "$(dirname -- "$rel")" 2>/dev/null && pwd)/$(basename -- "$rel") || continue
        [[ -f $included ]] || continue
        [[ -n ${seen[$included]:-} ]] && continue
        seen[$included]=1
        sources+=("$included")
        queue+=("$included")
    done < <(
        grep -oE '#\[path[[:space:]]*=[[:space:]]*"[^"]+"' "$current" |
            sed -E 's/^.*"([^"]+)"$/\1/'
    )
done

# An include-discovery bug is invisible — it silently degrades the cache key
# back to rust-script's main-script-only behaviour, which is the exact trap this
# launcher exists to close. Fail closed instead: a script that declares
# `#[path]` includes must have resolved at least one.
if grep -qE '#\[path[[:space:]]*=' "$script" && ((${#sources[@]} < 2)); then
    die "internal: $script declares #[path] includes but none resolved; the cache key would be unsound"
fi

cache_key=$(
    {
        printf '%s\0' "$(rustc --version)" "$edition" "$self_test" "$dependencies"
        # Sorted so the key does not depend on include-discovery order.
        printf '%s\n' "${sources[@]}" | LC_ALL=C sort | while IFS= read -r src; do
            printf '%s\0' "$src"
            cat -- "$src"
            printf '\0'
        done
    } | sha256sum | cut -c1-32
)

build_root="$ROOT_DIR/target/ci/rust-script/$pkg_name"
binary="$build_root/$cache_key"
((self_test)) && binary="$binary.test"

function run_it {
    # `exec` replaces this process, so the EXIT trap never fires. Clean the
    # staging directory here or every cold build leaks one under target/ci.
    if [[ -n ${staging:-} ]]; then
        rm -rf -- "$staging"
        trap - EXIT
    fi
    if ((print_binary)); then
        printf '%s\n' "$binary"
        exit 0
    fi
    # Becoming the program is load-bearing: validate.rs re-execs itself into a
    # transient systemd scope and expects the pid a caller signals or waits on
    # to be its own, not a shell wrapper's.
    exec "$binary" "$@"
}

[[ -x $binary ]] && run_it "$@"

mkdir -p "$build_root"
staging=$(mktemp -d "$build_root/build.XXXXXX")
trap 'rm -rf -- "$staging"' EXIT

if [[ -z ${dependencies//[[:space:]]/} ]]; then
    # No declared dependencies: compile the source directly, exactly as the
    # existing in-tree launchers do. Cheaper than materialising a package, and
    # it keeps the dependency-free scripts on a path with no cargo/network
    # requirement at all.
    test_flag=()
    ((self_test)) && test_flag=(--test)
    RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-stable} rustc \
        --edition="$edition" -O "${test_flag[@]}" \
        "$script" -o "$staging/out" >&2
else
    command -v cargo >/dev/null 2>&1 ||
        die "LAUNCH FAULT — $script_stem declares dependencies but cargo is not on PATH; nothing ran"
    # An explicit empty [workspace] table makes the generated package its own
    # workspace root. Without it cargo walks up to hermit's root Cargo.toml and
    # refuses to build a package that is not a listed member.
    cat >"$staging/Cargo.toml" <<EOF
[workspace]

[package]
name = "$pkg_name"
version = "0.0.0"
edition = "$edition"
publish = false

[[bin]]
name = "$pkg_name"
# The source stays where it is on purpose: \`#[path = "lib/…"]\` includes
# resolve relative to the file's own directory, so moving or copying it would
# break every included module.
path = "$script"

$dependencies
EOF
    # One target directory per script, REUSED across cache keys. A fresh
    # directory per build would re-resolve the index and recompile every
    # dependency on each source edit — measured at ~11s for validate.rs, versus
    # a couple of seconds once the dependency graph is warm.
    export CARGO_TARGET_DIR="$build_root/target"
    cargo_flags=(--release --manifest-path "$staging/Cargo.toml")
    if ((self_test)); then
        cargo test "${cargo_flags[@]}" --no-run --message-format=json \
            >"$staging/build.json"
        built=$(
            grep -o '"executable":"[^"]*"' "$staging/build.json" |
                tail -1 | sed 's/.*:"//;s/"$//'
        )
        [[ -n $built ]] || die "cargo test --no-run produced no test binary for $script"
        cp -- "$built" "$staging/out"
    else
        # Build chatter belongs on stderr so a script's own stdout stays clean
        # for pipelines and for --print-binary.
        cargo build "${cargo_flags[@]}" >&2
        cp -- "$CARGO_TARGET_DIR/release/$pkg_name" "$staging/out"
    fi
fi

# Atomic publish: concurrent launchers racing on the same key are harmless
# because they produce byte-identical work and the rename is all-or-nothing.
chmod +x "$staging/out"
mv -f -- "$staging/out" "$binary"

run_it "$@"
