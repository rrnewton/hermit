#!/usr/bin/env bash
# Reject developer-specific homes and hostnames in tracked build/run files.

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT_DIR

is_build_or_run_file() {
    case "$1" in
        *.sh | *.bash | *.rs | *.py | *.toml | *.yml | *.yaml | *.config \
            | *.conf | *.mk | Makefile | Makefile.* | */Makefile | */Makefile.* \
            | .github/* | ci/*)
            return 0 ;;
        *) return 1 ;;
    esac
}

is_excluded() {
    [[ $1 == ci/compat-envelope/cells.json ]] && return 0
    case "/$1/" in
        */.git/* | */ignored/* | */experiments/* | */scratch/* | */target/* \
            | */third-party/* | */vendor/* | */scripts/check-portable-paths.sh/)
            return 0 ;;
        *) return 1 ;;
    esac
}

scan_file() {
    local path=$1
    awk '
        {
            probe = tolower($0)
            gsub(/\/(home|users)\/(user|test|example)([^[:alnum:]_.-]|$)/,
                 "/generic/", probe)
            if (probe ~ /\/(home|users)\/[[:alnum:]_.-]+([^[:alnum:]_.-]|$)/ ||
                probe ~ /(^|[^[:alnum:]_])newton([^[:alnum:]_]|$)/ ||
                probe ~ /devbig[[:alnum:]._-]*/) {
                print FNR ":" $0
                found = 1
            }
        }
        END { exit found ? 1 : 0 }
    ' "$path"
}

check_repository() {
    local found=0
    local hit_file
    local path
    hit_file=$(mktemp "$ROOT_DIR/target/portable-path-hit.XXXXXX")
    while IFS= read -r -d '' path; do
        is_excluded "$path" && continue
        [[ -f $ROOT_DIR/$path ]] || continue
        is_build_or_run_file "$path" || [[ -x $ROOT_DIR/$path ]] || continue
        if ! scan_file "$ROOT_DIR/$path" >"$hit_file"; then
            while IFS= read -r hit; do
                printf '%s:%s\n' "$path" "$hit"
            done <"$hit_file"
            found=1
        fi
    done < <(git -C "$ROOT_DIR" ls-files -z)
    rm -f "$hit_file"
    return "$found"
}

self_test() {
    local fixture
    fixture=$(mktemp)

    printf '%s\n' "cache_dir=\"\${HOME}/.cache/hermit\"" >"$fixture"
    scan_file "$fixture" >/dev/null || {
        echo "portability self-test rejected a HOME-relative path" >&2
        rm -f "$fixture"
        return 1
    }

    printf 'cache_dir="/home/ci-portability-owner/.cache/hermit"\n' >"$fixture"
    if scan_file "$fixture" >/dev/null; then
        echo "portability self-test failed to reject a literal developer home" >&2
        rm -f "$fixture"
        return 1
    fi

    is_excluded ci/compat-envelope/cells.json || {
        echo "portability self-test failed to exclude literal compatibility evidence" >&2
        rm -f "$fixture"
        return 1
    }
    if is_excluded ci/compat-envelope/scorecard.rs; then
        echo "portability self-test excluded live compatibility code" >&2
        rm -f "$fixture"
        return 1
    fi
    if is_excluded archived/ci/compat-envelope/cells.json; then
        echo "portability self-test widened the literal evidence exclusion" >&2
        rm -f "$fixture"
        return 1
    fi
    if is_build_or_run_file fixtures/evidence.txt; then
        echo "portability self-test unexpectedly scans arbitrary text evidence" >&2
        rm -f "$fixture"
        return 1
    fi
    rm -f "$fixture"
}

mkdir -p "$ROOT_DIR/target"
self_test
if ! check_repository; then
    echo "portability check failed: replace literal homes/hosts with HOME, repo-relative paths, PATH lookup, or an explicit environment override" >&2
    # WARNING -- THE FIFTH REMEDY, AND IT EXISTS BECAUSE FOUR AGENTS GUESSED A
    # SIXTH. On 2026-08-25 five heads cleared this gate by removing a host
    # identity from a provenance record; four destroyed the information (#2646,
    # #2647, #2648, #2652) and one moved it correctly (#2526). Every remedy on
    # the line above substitutes something a PROGRAM resolves -- none fits a
    # hostname in a COMMENT recording where a measurement was taken, where the
    # literal is a fact rather than a path. Faced with four inapplicable options
    # and a red gate, the cheapest remaining action is deletion, and a
    # measurement whose host is unrecorded cannot be re-run, compared or
    # challenged. Naming the fifth option is what stops that.
    echo "  ...or, when the literal is a MEASUREMENT PROVENANCE rather than a path: record the host in docs/TESTING_ENVIRONMENTS.md (this checker does not scan docs/) and reference it from here." >&2
    echo "  DO NOT DELETE IT. A measurement with no host cannot be re-run, compared or challenged; that is what four fixes on 2026-08-25 cost." >&2
    exit 1
fi

echo "Portability path check passed."
