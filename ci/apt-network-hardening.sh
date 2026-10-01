#!/usr/bin/env bash
# apt-network-hardening.sh -- keep a stalled Ubuntu mirror from spending a
# hosted CI job's time limit, and keep a second mirror behind it.
#
# BACKGROUND. GitHub-hosted ubuntu-24.04 runners download Ubuntu packages from
# azure.archive.ubuntu.com, a mirror inside Azure. Each job in
# .github/workflows/ci-portable.yml installs its tools in a step named
# "Dependencies" (`apt-get update`, then `apt-get install`), and that time
# counts against the job's timeout-minutes. When the mirror accepts a request
# and then sends nothing, apt (2.7.14, the noble release) behaves as follows:
#
#   * methods/http.cc reads Acquire::http::Timeout (or ::https::) once per
#     connection; an idle wait that long is a transient error.
#   * methods/basehttp.cc fails a header read only on the second consecutive
#     error, so one stalled attempt costs two of those waits.
#   * apt-pkg/acquire-worker.cc retries a transient failure on the SAME mirror
#     while Acquire::Retries is not exhausted, and only then moves to the next
#     mirror; the count is per item and is not reset by the move
#     (apt-pkg/acquire-item.cc).
#   * The mirror method moves to the next entry of a `mirror+file:` list when
#     a fetch fails, lowest `priority:` first, until none are left
#     (apt-transport-mirror(1),
#     https://manpages.ubuntu.com/manpages/noble/man1/apt-transport-mirror.1.html).
#
# The runner image already serves the archive through
# mirror+file:/etc/apt/apt-mirrors.txt (Azure, then archive.ubuntu.com, then
# security.ubuntu.com) and sets Acquire::Retries "1" with 15-second timeouts in
# /etc/apt/apt.conf.d/zz-retries
# (https://github.com/actions/runner-images/pull/14726, for
# https://github.com/actions/runner-images/issues/14594). One stalled package
# therefore costs (1 + 1 retry) x 2 x 15 s = 60 s on Azure before the fallback
# serves it.
#
# WHAT THIS DOES, in order:
#   1. Classifies /etc/apt/sources.list.d/ubuntu.sources (deb822):
#      a. every archive URIs field is mirror+file:<list>, and each list names
#         at least one non-Azure http(s) mirror: kept as found;
#      b. stock `URIs: http://azure.archive.ubuntu.com/ubuntu/`: those fields
#         are pointed at mirror+file:/etc/apt/hermit-ci-mirrors.txt, which
#         lists Azure first and http://archive.ubuntu.com/ubuntu/ second;
#      c. anything else: ONE warning and nothing is changed, including step 2,
#         because a short timeout with nowhere to fail over to turns a slow
#         mirror into a failed install.
#      URIs fields on security.ubuntu.com are never modified.
#   2. Writes /etc/apt/apt.conf.d/zzz-hermit-ci-network, which apt reads after
#      the image's zz-retries (apt.conf.d is read in byte order and the last
#      value wins): Acquire::Retries "1" (the image's value, pinned) and
#      Acquire::http::Timeout / Acquire::https::Timeout "5". A stalled package
#      then costs (1 + 1) x 2 x 5 s = 20 s before the fallback serves it. The
#      wait is an IDLE wait that restarts on every byte received, so a slow
#      transfer that is still delivering data is never cut off by it.
#
# NOT ADDRESSED. apt has no minimum-throughput setting. A mirror that keeps
# sending at ~125 kB/s never trips the idle wait and never fails over.
#
# CONTRACT. Idempotent: a second run changes nothing. Always exits 0 apart from
# a usage error (exit 2): a failure inside the work prints one warning and
# leaves apt with the image's configuration for every file not yet replaced
# (each file is replaced atomically). Prints no environment and no URI from
# the inspected files.
#
# Usage: sudo ci/apt-network-hardening.sh [--root DIR]
#   --root DIR, or HERMIT_APT_HARDENING_ROOT=DIR, operates on DIR/etc/apt
#   instead of /etc/apt, so the tests run without root.
# Self-test: ci/apt-network-hardening-test.sh (Makefile `lint-checks`).

set -uo pipefail

readonly PROG=apt-network-hardening
readonly AZURE_URI=http://azure.archive.ubuntu.com/ubuntu/
readonly FALLBACK_URI=http://archive.ubuntu.com/ubuntu/
readonly OWN_LIST=/etc/apt/hermit-ci-mirrors.txt
readonly OWN_CONF=/etc/apt/apt.conf.d/zzz-hermit-ci-network
readonly SOURCES=/etc/apt/sources.list.d/ubuntu.sources

usage() {
    printf 'usage: %s [--root DIR]\n' "$0" >&2
    exit 2
}

root=${HERMIT_APT_HARDENING_ROOT:-}
while [[ $# -gt 0 ]]; do
    case $1 in
        --root)
            [[ $# -ge 2 && -n $2 ]] || usage
            root=$2
            shift 2
            ;;
        *) usage ;;
    esac
done
root=${root%/}

say() { printf '%s: %s\n' "$PROG" "$*"; }
# The one warning a run may print; GitHub renders it as an annotation.
warn() { printf '::warning title=%s::%s\n' "$PROG" "$*"; }

own_list_content() {
    printf '%s\tpriority:1\n%s\tpriority:2\n' "$AZURE_URI" "$FALLBACK_URI"
}

own_conf_content() {
    printf '%s\n' \
        '// Written by ci/apt-network-hardening.sh for hosted CI runners.' \
        '// Sorts after the image'"'"'s zz-retries so these values win.' \
        'Acquire::Retries "1";' \
        'Acquire::http::Timeout "5";' \
        'Acquire::https::Timeout "5";'
}

# Replace LOGICAL_PATH (under $root) with stdin only if the bytes differ.
# Prints "written" or "unchanged". The temporary file lives in the parent of
# apt.conf.d and sources.list.d, where apt reads no wildcard.
replace_if_changed() {
    local target="$root$1" tmp ok=1
    tmp=$(mktemp "$root/etc/apt/.$PROG.XXXXXX") || return 1
    cat >"$tmp" || ok=0
    if [[ $ok -eq 1 && -f $target ]] && cmp -s -- "$tmp" "$target"; then
        rm -f -- "$tmp"
        echo unchanged
        return 0
    fi
    if [[ $ok -eq 1 && -e $target ]]; then
        chmod --reference="$target" -- "$tmp" || ok=0
    elif [[ $ok -eq 1 ]]; then
        chmod 0644 -- "$tmp" || ok=0
    fi
    if [[ $ok -eq 1 ]]; then
        mv -f -- "$tmp" "$target" || ok=0
    fi
    if [[ $ok -eq 0 ]]; then
        rm -f -- "$tmp"
        return 1
    fi
    echo written
}

# One line per URIs field: AZ, SEC, MIRROR <path>, or OTHER.
classify_uris() {
    awk '
        function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
        {
            if (in_uris && $0 ~ /^[ \t]+[^ \t\r]/) print "OTHER"
            in_uris = 0
            if ($0 ~ /^#/) next
            if (tolower($0) ~ /^uris:/) {
                in_uris = 1
                v = trim(substr($0, 6))
                if (v == "" || v ~ /[ \t]/) print "OTHER"
                else if (v == "http://azure.archive.ubuntu.com/ubuntu/" ||
                         v == "http://azure.archive.ubuntu.com/ubuntu") print "AZ"
                else if (v ~ /^https?:\/\/security\.ubuntu\.com\/ubuntu\/?$/) print "SEC"
                else if (v ~ /^mirror\+file:\/[^ \t]+$/) print "MIRROR " substr(v, 13)
                else print "OTHER"
            }
        }' "$1"
}

# "<entries> <non-Azure http(s) entries>" for a mirror list.
count_mirrors() {
    awk '
        /^[ \t]*(#|$)/ { next }
        {
            total++
            split($0, f, /[ \t]/)
            if (f[1] ~ /^https?:\/\// && f[1] !~ /^https?:\/\/azure\.archive\.ubuntu\.com\//) n++
        }
        END { print total + 0, n + 0 }' "$1"
}

rewrite_azure_uris() {
    awk -v own="mirror+file:$OWN_LIST" '
        tolower($0) ~ /^uris:[ \t]*http:\/\/azure\.archive\.ubuntu\.com\/ubuntu\/?[ \t\r]*$/ {
            print "URIs: " own
            next
        }
        { print }' "$1"
}

harden() {
    local sources="$root$SOURCES" kinds n_az n_mirror n_other list total fallbacks status

    if [[ ! -f $sources ]]; then
        warn "unexpected apt layout: no $SOURCES; apt configuration left unchanged"
        return 0
    fi
    kinds=$(classify_uris "$sources")
    n_az=$(grep -c '^AZ$' <<<"$kinds" || true)
    n_mirror=$(grep -c '^MIRROR ' <<<"$kinds" || true)
    n_other=$(grep -c '^OTHER$' <<<"$kinds" || true)

    if [[ $n_other -gt 0 ]]; then
        warn "unexpected apt layout: $n_other URIs field(s) in $SOURCES are not a single Azure, security.ubuntu.com or mirror+file URI; apt configuration left unchanged"
        return 0
    fi
    if [[ $n_az -gt 0 && $n_mirror -gt 0 ]]; then
        warn "unexpected apt layout: $SOURCES mixes direct Azure and mirror+file URIs; apt configuration left unchanged"
        return 0
    fi
    if [[ $n_az -eq 0 && $n_mirror -eq 0 ]]; then
        warn "unexpected apt layout: $SOURCES has no Azure or mirror+file URI; apt configuration left unchanged"
        return 0
    fi

    if [[ $n_mirror -gt 0 ]]; then
        while read -r list; do
            if [[ ! -f $root$list ]]; then
                warn "unexpected apt layout: mirror list $list is missing; apt configuration left unchanged"
                return 0
            fi
            read -r total fallbacks < <(count_mirrors "$root$list")
            if [[ $fallbacks -eq 0 ]]; then
                warn "unexpected apt layout: mirror list $list has no non-Azure mirror; apt configuration left unchanged"
                return 0
            fi
            say "Ubuntu archive already fails over through mirror+file:$list ($total mirrors, $fallbacks not Azure)"
        done < <(sed -n 's/^MIRROR //p' <<<"$kinds" | sort -u)
    else
        status=$(own_list_content | replace_if_changed "$OWN_LIST")
        say "$OWN_LIST $status (Azure, then $FALLBACK_URI)"
        status=$(rewrite_azure_uris "$sources" | replace_if_changed "$SOURCES")
        say "$SOURCES $status: $n_az Azure URIs field(s) now mirror+file:$OWN_LIST"
    fi

    status=$(own_conf_content | replace_if_changed "$OWN_CONF")
    say "$OWN_CONF $status (Acquire::Retries 1, Acquire::http(s)::Timeout 5)"

    if [[ -z $root ]] && command -v apt-config >/dev/null 2>&1; then
        say "effective values:"
        apt-config dump Acquire::Retries Acquire::http::Timeout Acquire::https::Timeout || true
    fi
}

# `set -e` applies inside the subshell only because the subshell's status is
# not tested by `||`, `&&` or `if`; bash disables `set -e` in such contexts.
# inherit_errexit carries it into `$(...)`, which would otherwise drop it.
(
    set -e
    shopt -s inherit_errexit
    harden
)
rc=$?
if [[ $rc -ne 0 ]]; then
    warn "stopped early with status $rc; files not yet replaced keep the image's apt configuration"
fi
exit 0
