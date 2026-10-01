#!/usr/bin/env bash
# Self-test for ci/apt-network-hardening.sh, run by the Makefile's
# `lint-checks` recipe (DAG node check.lint_checks).
#
# Every expected file below is written out literally rather than produced by
# the script's own helpers: a test that reused the implementation's text would
# only prove the script agrees with itself. Each case runs the real script with
# --root against a fixture tree, so no root access is needed.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
SCRIPT="$HERE/apt-network-hardening.sh"

failures=0
check() { # check <description> <condition-result>
    if [[ $2 -eq 0 ]]; then echo "ok   - $1"; else echo "FAIL - $1"; failures=$((failures+1)); fi
}

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# ----------------------------------------------------------------- fixtures --
T=$'\t'
SIGNED='Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg'
stanza() { # stanza <uris-line> <suites>
    printf 'Types: deb\n%s\nSuites: %s\nComponents: main restricted universe multiverse\n%s\n' \
        "$1" "$2" "$SIGNED"
}

# The ubuntu-24.04 runner image as measured: the archive (security pocket
# included) already goes through the image's mirror list.
IMAGE_SOURCES="$(stanza 'URIs: mirror+file:/etc/apt/apt-mirrors.txt' 'noble noble-updates noble-backports')

$(stanza 'URIs: mirror+file:/etc/apt/apt-mirrors.txt' 'noble-security')
"
IMAGE_MIRRORS="http://azure.archive.ubuntu.com/ubuntu/${T}priority:1
https://archive.ubuntu.com/ubuntu/${T}priority:2
https://security.ubuntu.com/ubuntu/${T}priority:3
"
IMAGE_RETRIES='Acquire::Retries "1";
Acquire::http::Timeout "15";
Acquire::https::Timeout "15";
'

# A stock Azure deb822 file with the security pocket on security.ubuntu.com,
# and the same file after the rewrite: only the Azure URIs line changes.
STOCK_SOURCES="# URIs: http://azure.archive.ubuntu.com/ubuntu/ (comment, never rewritten)
$(stanza 'URIs: http://azure.archive.ubuntu.com/ubuntu/' 'noble noble-updates noble-backports')

$(stanza 'URIs: http://security.ubuntu.com/ubuntu/' 'noble-security')
"
STOCK_EXPECTED="# URIs: http://azure.archive.ubuntu.com/ubuntu/ (comment, never rewritten)
$(stanza 'URIs: mirror+file:/etc/apt/hermit-ci-mirrors.txt' 'noble noble-updates noble-backports')

$(stanza 'URIs: http://security.ubuntu.com/ubuntu/' 'noble-security')
"
# Azure in both stanzas, once without the trailing slash.
BOTH_SOURCES="$(stanza 'URIs: http://azure.archive.ubuntu.com/ubuntu/' 'noble noble-updates')

$(stanza 'URIs: http://azure.archive.ubuntu.com/ubuntu' 'noble-security')
"
BOTH_EXPECTED="$(stanza 'URIs: mirror+file:/etc/apt/hermit-ci-mirrors.txt' 'noble noble-updates')

$(stanza 'URIs: mirror+file:/etc/apt/hermit-ci-mirrors.txt' 'noble-security')
"
OWN_MIRRORS_EXPECTED="http://azure.archive.ubuntu.com/ubuntu/${T}priority:1
http://archive.ubuntu.com/ubuntu/${T}priority:2
"
CONF_EXPECTED='// Written by ci/apt-network-hardening.sh for hosted CI runners.
// Sorts after the image'"'"'s zz-retries so these values win.
Acquire::Retries "1";
Acquire::http::Timeout "5";
Acquire::https::Timeout "5";
'

new_tree() { # new_tree <name> <ubuntu.sources content or "-" for none>
    local dir="$work/$1"
    mkdir -p "$dir/etc/apt/apt.conf.d" "$dir/etc/apt/sources.list.d"
    printf '%s' "$IMAGE_RETRIES" >"$dir/etc/apt/apt.conf.d/zz-retries"
    printf '%s\n' '# Ubuntu sources have moved to /etc/apt/sources.list.d/ubuntu.sources' \
        >"$dir/etc/apt/sources.list"
    if [[ $2 != - ]]; then
        printf '%s' "$2" >"$dir/etc/apt/sources.list.d/ubuntu.sources"
    fi
    printf '%s\n' "$dir"
}

# Names, types and modes of every entry, plus size, nanosecond mtime and
# content hash of every file: a rerun that rewrites a file with identical bytes
# still changes this. Directory mtimes are left out because the script's
# comparison scratch file is created and removed in /etc/apt on every run.
snapshot() {
    find "$1" -printf '%P %y %m\n' | LC_ALL=C sort
    find "$1" -type f -printf '%P %s %T@\n' | LC_ALL=C sort
    find "$1" -type f -exec sha256sum {} + | LC_ALL=C sort
}

run() { # run <tree> [extra args...]; sets $out and $rc
    local tree=$1; shift
    out=$("$SCRIPT" --root "$tree" "$@" 2>&1); rc=$?
}

warnings() { grep -c '^::warning' <<<"$out"; }
same_file() { cmp -s -- "$1" <(printf '%s' "$2"); }

check "script is executable (the workflow runs it as ./ci/...)" "$([[ -x $SCRIPT ]]; echo $?)"
check "own conf name sorts after the image's zz-retries in byte order" \
    "$(printf 'zz-retries\nzzz-hermit-ci-network\n' | LC_ALL=C sort -c 2>/dev/null; echo $?)"

# ------------------------------------------------- shape a: runner image --
tree=$(new_tree image "$IMAGE_SOURCES")
printf '%s' "$IMAGE_MIRRORS" >"$tree/etc/apt/apt-mirrors.txt"
before=$(snapshot "$tree" | grep -v 'zzz-hermit-ci-network')
run "$tree"
check "image: exit 0" "$([[ $rc -eq 0 ]]; echo $?)"
check "image: no warning" "$([[ $(warnings) -eq 0 ]]; echo $?)"
check "image: reports the existing fallback and its size" "$(grep -q 'already fails over through mirror+file:/etc/apt/apt-mirrors.txt (3 mirrors, 2 not Azure)' <<<"$out"; echo $?)"
check "image: sources, mirror list and zz-retries untouched" \
    "$([[ $(snapshot "$tree" | grep -v 'zzz-hermit-ci-network') == "$before" ]]; echo $?)"
check "image: conf written with the exact expected bytes" \
    "$(same_file "$tree/etc/apt/apt.conf.d/zzz-hermit-ci-network" "$CONF_EXPECTED"; echo $?)"
check "image: conf mode 0644" "$([[ $(stat -c %a "$tree/etc/apt/apt.conf.d/zzz-hermit-ci-network") == 644 ]]; echo $?)"
check "image: own mirror list not created" "$([[ ! -e $tree/etc/apt/hermit-ci-mirrors.txt ]]; echo $?)"
first=$(snapshot "$tree")
run "$tree"
check "image rerun: exit 0, no warning" "$([[ $rc -eq 0 && $(warnings) -eq 0 ]]; echo $?)"
check "image rerun: reports unchanged" "$(grep -q 'zzz-hermit-ci-network unchanged' <<<"$out"; echo $?)"
check "image rerun: tree identical, mtimes included" "$([[ $(snapshot "$tree") == "$first" ]]; echo $?)"

# Same tree through the environment variable instead of --root.
tree=$(new_tree image-env "$IMAGE_SOURCES")
printf '%s' "$IMAGE_MIRRORS" >"$tree/etc/apt/apt-mirrors.txt"
out=$(HERMIT_APT_HARDENING_ROOT="$tree" "$SCRIPT" 2>&1); rc=$?
check "env root: exit 0, no warning" "$([[ $rc -eq 0 && $(warnings) -eq 0 ]]; echo $?)"
check "env root: conf written" \
    "$(same_file "$tree/etc/apt/apt.conf.d/zzz-hermit-ci-network" "$CONF_EXPECTED"; echo $?)"

# --------------------------------------------- shape b: stock Azure source --
tree=$(new_tree stock "$STOCK_SOURCES")
chmod 0640 "$tree/etc/apt/sources.list.d/ubuntu.sources"
run "$tree"
check "stock: exit 0" "$([[ $rc -eq 0 ]]; echo $?)"
check "stock: no warning" "$([[ $(warnings) -eq 0 ]]; echo $?)"
check "stock: only the Azure URIs line rewritten; comment and security.ubuntu.com untouched" \
    "$(same_file "$tree/etc/apt/sources.list.d/ubuntu.sources" "$STOCK_EXPECTED"; echo $?)"
check "stock: rewritten sources keep their mode" \
    "$([[ $(stat -c %a "$tree/etc/apt/sources.list.d/ubuntu.sources") == 640 ]]; echo $?)"
check "stock: mirror list is Azure priority 1, archive.ubuntu.com priority 2" \
    "$(same_file "$tree/etc/apt/hermit-ci-mirrors.txt" "$OWN_MIRRORS_EXPECTED"; echo $?)"
check "stock: conf written with the exact expected bytes" \
    "$(same_file "$tree/etc/apt/apt.conf.d/zzz-hermit-ci-network" "$CONF_EXPECTED"; echo $?)"
check "stock: zz-retries untouched" \
    "$(same_file "$tree/etc/apt/apt.conf.d/zz-retries" "$IMAGE_RETRIES"; echo $?)"
check "stock: no temporary file left" \
    "$([[ -z $(find "$tree" -name '.apt-network-hardening.*') ]]; echo $?)"
first=$(snapshot "$tree")
run "$tree"
check "stock rerun: exit 0, no warning" "$([[ $rc -eq 0 && $(warnings) -eq 0 ]]; echo $?)"
check "stock rerun: now recognised as already failing over" \
    "$(grep -q 'already fails over through mirror+file:/etc/apt/hermit-ci-mirrors.txt (2 mirrors, 1 not Azure)' <<<"$out"; echo $?)"
check "stock rerun: tree identical, mtimes included" "$([[ $(snapshot "$tree") == "$first" ]]; echo $?)"

tree=$(new_tree both "$BOTH_SOURCES")
run "$tree"
check "azure in both stanzas: exit 0, no warning" "$([[ $rc -eq 0 && $(warnings) -eq 0 ]]; echo $?)"
check "azure in both stanzas: both rewritten, trailing slash or not" \
    "$(same_file "$tree/etc/apt/sources.list.d/ubuntu.sources" "$BOTH_EXPECTED"; echo $?)"

# ------------------------------------------ shape c: unexpected, unchanged --
unexpected() { # unexpected <name> <tree>
    local tree=$2 before
    before=$(snapshot "$tree")
    run "$tree"
    check "$1: exit 0" "$([[ $rc -eq 0 ]]; echo $?)"
    check "$1: exactly one warning" "$([[ $(warnings) -eq 1 ]]; echo $?)"
    check "$1: tree unchanged (no conf, no rewrite)" "$([[ $(snapshot "$tree") == "$before" ]]; echo $?)"
}

# Beside an Azure stanza, so that tolerating the unknown mirror would show up
# as a rewrite rather than pass through another refusal.
unexpected "other mirror beside Azure" "$(new_tree other "$(stanza 'URIs: http://azure.archive.ubuntu.com/ubuntu/' noble)

$(stanza 'URIs: http://us.archive.ubuntu.com/ubuntu/' noble-updates)
")"
unexpected "two URIs in one field" "$(new_tree multi "$(stanza 'URIs: http://azure.archive.ubuntu.com/ubuntu/ http://archive.ubuntu.com/ubuntu/' noble)
")"
unexpected "URIs continuation line" "$(new_tree continued "Types: deb
URIs: http://azure.archive.ubuntu.com/ubuntu/
 http://archive.ubuntu.com/ubuntu/
Suites: noble
")"
unexpected "no ubuntu.sources" "$(new_tree missing -)"
unexpected "security.ubuntu.com only" "$(new_tree security-only "$(stanza 'URIs: http://security.ubuntu.com/ubuntu/' noble-security)
")"
unexpected "direct Azure mixed with mirror+file" "$(new_tree mixed "$IMAGE_SOURCES
$(stanza 'URIs: http://azure.archive.ubuntu.com/ubuntu/' noble-proposed)
")"
tree=$(new_tree azure-only-list "$IMAGE_SOURCES")
printf 'http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n' >"$tree/etc/apt/apt-mirrors.txt"
unexpected "mirror list without a non-Azure mirror" "$tree"
unexpected "mirror list missing" "$(new_tree no-list "$IMAGE_SOURCES")"

tree=$(new_tree credentials "$(stanza 'URIs: https://user:s3cr3t-token@mirror.example.invalid/ubuntu/' noble)
")
run "$tree"
check "a URI from the inspected file is never printed" "$(! grep -q 's3cr3t-token' <<<"$out"; echo $?)"

# ------------------------------------------------- internal failure path --
# apt.conf.d as a regular file makes the final rename fail even for root.
tree="$work/broken"
mkdir -p "$tree/etc/apt/sources.list.d"
printf '%s' "$IMAGE_SOURCES" >"$tree/etc/apt/sources.list.d/ubuntu.sources"
printf '%s' "$IMAGE_MIRRORS" >"$tree/etc/apt/apt-mirrors.txt"
: >"$tree/etc/apt/apt.conf.d"
run "$tree"
check "failure inside the work: still exit 0" "$([[ $rc -eq 0 ]]; echo $?)"
check "failure inside the work: exactly one warning, naming the early stop" \
    "$([[ $(warnings) -eq 1 ]] && grep -q '^::warning.*stopped early' <<<"$out"; echo $?)"
check "failure inside the work: no temporary file left" \
    "$([[ -z $(find "$tree" -name '.apt-network-hardening.*') ]]; echo $?)"

# ------------------------------------------------------------------ usage --
"$SCRIPT" --bogus >/dev/null 2>&1; rc=$?
check "unknown option: exit 2" "$([[ $rc -eq 2 ]]; echo $?)"
"$SCRIPT" --root >/dev/null 2>&1; rc=$?
check "--root without a directory: exit 2" "$([[ $rc -eq 2 ]]; echo $?)"

if [[ $failures -ne 0 ]]; then
    echo "apt-network-hardening-test: $failures check(s) failed" >&2
    exit 1
fi
echo "apt-network-hardening-test: all checks passed"
